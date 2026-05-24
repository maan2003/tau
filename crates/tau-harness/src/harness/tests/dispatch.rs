use super::*;
use crate::conversation::{Conversation, ConversationId, PendingPrompt};
use crate::harness::{
    PendingTool, background_completion_prompt,
    extension_disconnected_background_tool_call_error_message,
    extension_disconnected_tool_call_error_message, is_restore_notice_prompt_text,
    restore_notice_prompt_for_elapsed, unavailable_tool_error_message,
};

fn responses_backend() -> tau_proto::ProviderBackend {
    tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::Responses,
        base_url: "https://api.example.test".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    }
}

fn assert_delegate_tools_counter(
    progress: &tau_proto::DelegateProgress,
    complete: Option<u64>,
    total: Option<u64>,
) {
    let display = progress
        .display
        .as_ref()
        .expect("delegate progress display");
    let counter = display
        .progress_counters
        .iter()
        .find(|counter| counter.label.as_deref() == Some("tools"))
        .expect("tools progress counter");
    assert_eq!(counter.complete, complete);
    assert_eq!(counter.total, total);
}

fn assert_delegate_input_stats(
    progress: &tau_proto::DelegateProgress,
    expected: tau_proto::ToolDisplayStats,
) {
    let display = progress
        .display
        .as_ref()
        .expect("delegate progress display");
    assert_eq!(display.stats, expected);
}

fn assert_delegate_counter_order(progress: &tau_proto::DelegateProgress, labels: &[&str]) {
    let display = progress
        .display
        .as_ref()
        .expect("delegate progress display");
    let actual: Vec<&str> = display
        .progress_counters
        .iter()
        .map(|counter| counter.label.as_deref().expect("progress label"))
        .collect();
    assert_eq!(actual, labels);
}

fn assert_delegate_ctx_counter(
    progress: &tau_proto::DelegateProgress,
    complete: Option<u64>,
    total: Option<u64>,
) {
    let display = progress
        .display
        .as_ref()
        .expect("delegate progress display");
    let counter = display
        .progress_counters
        .iter()
        .find(|counter| counter.label.as_deref() == Some("ctx"))
        .expect("ctx progress counter");
    assert_eq!(counter.unit, tau_proto::ProgressUnit::Tokens);
    assert_eq!(counter.complete, complete);
    assert_eq!(counter.total, total);
}

fn text_part(item: &ContextItem) -> Option<&str> {
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

fn openai_compaction_summary_item(text: &str) -> ContextItem {
    ContextItem::Compaction(tau_proto::OpaqueProviderItem(CborValue::Map(vec![
        (
            CborValue::Text("type".to_owned()),
            CborValue::Text("message".to_owned()),
        ),
        (
            CborValue::Text("role".to_owned()),
            CborValue::Text("assistant".to_owned()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("type".to_owned()),
                    CborValue::Text("output_text".to_owned()),
                ),
                (
                    CborValue::Text("text".to_owned()),
                    CborValue::Text(text.to_owned()),
                ),
            ])]),
        ),
    ])))
}

fn tool_call_id(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::ToolCall(call) => Some(call.call_id.as_str()),
        _ => None,
    }
}

fn tool_result_id(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::ToolResult(result) => Some(result.call_id.as_str()),
        _ => None,
    }
}

fn cbor_map_text<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(entry_key, entry_value)| {
        matches!(entry_key, CborValue::Text(text) if text == key)
            .then_some(entry_value)
            .and_then(|value| match value {
                CborValue::Text(text) => Some(text.as_str()),
                _ => None,
            })
    })
}

fn provider_text_response(spid: &SessionPromptId, text: &str) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id: spid.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn seed_prior_user_message(state_dir: &Path, text: &str) {
    seed_prior_user_message_at(state_dir, text, tau_proto::UnixMicros::now());
}

fn seed_prior_user_message_at(state_dir: &Path, text: &str, recorded_at: tau_proto::UnixMicros) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event_at(
            "s1",
            None,
            None,
            Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: text.to_owned(),
                target_agent_id: None,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
            recorded_at,
        )
        .expect("seed prior user message");
}

fn context_text_count(prompt: &SessionPromptCreated, text: &str) -> usize {
    prompt
        .context_items
        .iter()
        .filter(|item| text_part(item) == Some(text))
        .count()
}

fn restore_notice_context_text(prompt: &SessionPromptCreated) -> Option<&str> {
    prompt
        .context_items
        .iter()
        .filter_map(text_part)
        .find(|text| is_restore_notice_prompt_text(text))
}

fn restore_notice_context_count(prompt: &SessionPromptCreated) -> usize {
    prompt
        .context_items
        .iter()
        .filter_map(text_part)
        .filter(|text| is_restore_notice_prompt_text(text))
        .count()
}

fn restore_notice_event_count(h: &Harness) -> usize {
    h.store
        .session_events("s1")
        .expect("session events")
        .iter()
        .filter(|entry| {
            matches!(
                &entry.event,
                Event::UiPromptSubmitted(prompt)
                    if prompt.message_class.is_internal()
                        && is_restore_notice_prompt_text(&prompt.text)
            )
        })
        .count()
}

fn restored_background_notice(call_id: &str) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn seed_background_placeholder(state_dir: &Path, call_id: &str, tool_name: &str) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event(
            "s1",
            None,
            Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: format!("run {tool_name}"),
                target_agent_id: None,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
        )
        .expect("seed prior user message");
    store
        .append_session_event(
            "s1",
            None,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                session_prompt_id: format!("sp-{call_id}").into(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.into(),
                    name: ToolName::new(tool_name),
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
        )
        .expect("seed background tool call");
    store
        .append_session_event(
            "s1",
            None,
            Event::ProviderToolResult(ToolResult {
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text(format!(
                    "{}: true\n\nTool call `{call_id}` is running in the background.",
                    tau_proto::TAU_INTERNAL_HEADER_NAME
                )),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("seed background placeholder");
}

fn background_error_count(h: &Harness, call_id: &str) -> usize {
    h.store
        .session_events("s1")
        .expect("session events")
        .iter()
        .filter(|entry| {
            matches!(&entry.event, Event::ToolBackgroundError(error) if error.call_id.as_str() == call_id)
        })
        .count()
}

fn background_result_count(h: &Harness, call_id: &str) -> usize {
    h.store
        .session_events("s1")
        .expect("session events")
        .iter()
        .filter(|entry| {
            matches!(&entry.event, Event::ToolBackgroundResult(result) if result.call_id.as_str() == call_id)
        })
        .count()
}

fn background_placeholder_count(h: &Harness, call_id: &str) -> usize {
    h.store
        .session_events("s1")
        .expect("session events")
        .iter()
        .filter(|entry| {
            matches!(
                &entry.event,
                Event::ProviderToolResult(result)
                    if result.call_id.as_str() == call_id
                        && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            )
        })
        .count()
}

fn event_log_contains(h: &Harness, source: &str, matches_event: impl Fn(&Event) -> bool) -> bool {
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if entry.source.as_deref() == Some(source) && matches_event(&entry.event) {
            return true;
        }
    }
    false
}

fn event_log_position(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> Option<u64> {
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if matches_event(&entry.event) {
            return Some(entry.seq);
        }
    }
    None
}

fn event_log_position_after(
    h: &Harness,
    after_seq: u64,
    matches_event: impl Fn(&Event) -> bool,
) -> Option<u64> {
    let mut seq = after_seq + 1;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if matches_event(&entry.event) {
            return Some(entry.seq);
        }
    }
    None
}

fn event_log_contains_any_source(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> bool {
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if matches_event(&entry.event) {
            return true;
        }
    }
    false
}

fn shared_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: None,
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    }
}

fn exclusive_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        execution_mode: ToolExecutionMode::Exclusive,
        ..shared_test_tool_spec(name)
    }
}

fn scheduled_test_tool_spec(
    name: &str,
    execution_mode: ToolExecutionMode,
    background_support: tau_proto::BackgroundSupport,
) -> ToolSpec {
    ToolSpec {
        execution_mode,
        background_support: Some(background_support),
        ..shared_test_tool_spec(name)
    }
}

fn setup_routed_test_tool_call(call_id: &str, tool_name: &str) -> (TempDir, Harness) {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-owner");
    let _ = connect_test_tool(&mut h, "conn-wrong");
    h.registry
        .register("conn-owner", shared_test_tool_spec(tool_name));

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = format!("sp-{call_id}").into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid);

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: ToolName::new(tool_name),
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
    .expect("tool call routed");
    assert_eq!(
        h.pending_tool_providers
            .get(call_id)
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );

    (td, h)
}

fn tool_invoke_call_ids(events: &Arc<Mutex<Vec<RoutedFrame>>>) -> Vec<String> {
    events
        .lock()
        .expect("sink mutex")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::ToolStarted(invoke)) => Some(invoke.call_id.to_string()),
            _ => None,
        })
        .collect()
}

/// Invalid model arguments must be rejected before the logical tool pipeline.
/// This keeps bad calls out of the event log as
/// `ToolRequest`/`ToolStarted` while still returning a
/// provider-facing tool error to the model.
#[test]
fn invalid_tool_arguments_are_rejected_before_logical_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-strict-tool");
    let mut spec = shared_test_tool_spec("strict_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "allowed": { "type": "string" }
        },
        "required": ["allowed"],
        "additionalProperties": false
    }));
    h.registry.register("conn-strict-tool", spec);

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-invalid-tool-args".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "bad-args".into(),
            name: ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("allowed".to_owned()),
                    CborValue::Text("ok".to_owned()),
                ),
                (
                    CborValue::Text("extra".to_owned()),
                    CborValue::Text("nope".to_owned()),
                ),
            ]),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut provider_error = None;
    let mut logical_events = Vec::new();
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        match &entry.event {
            Event::ProviderToolError(error) if error.call_id.as_str() == "bad-args" => {
                provider_error = Some(error.message.clone());
            }
            Event::ToolRequest(request) if request.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.request");
            }
            Event::ToolStarted(invoke) if invoke.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.started");
            }
            Event::ToolError(error) if error.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.error");
            }
            _ => {}
        }
    }

    let provider_error = provider_error.expect("provider tool error");
    assert!(provider_error.contains("invalid arguments for tool `strict_tool`"));
    assert!(provider_error.contains("unexpected argument `extra`"));
    assert!(
        logical_events.is_empty(),
        "unexpected events: {logical_events:?}"
    );
    assert!(tool_invoke_call_ids(&tool_events).is_empty());

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnect_with_inflight_and_queued_tool_does_not_invoke_queued_on_dead_connection() {
    // Regression: disconnect cleanup must unregister the provider before a
    // failed foreground call releases the scheduler. Otherwise the queued call
    // can be routed as `ToolStarted` to the already-dead connection.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-dead-tool");
    h.registry
        .register("conn-dead-tool", exclusive_test_tool_spec("dead_slow"));

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-dead-tool".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run two slow tools".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "running-call".into(),
                name: ToolName::new("dead_slow"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-call".into(),
                name: ToolName::new("dead_slow"),
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

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["running-call".to_owned()]
    );
    assert_eq!(h.tool_turn.pending_len(), 1);

    h.handle_disconnect("conn-dead-tool");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["running-call".to_owned()]
    );
    assert!(h.registry.providers_for("dead_slow").is_empty());
    assert!(h.tool_turn.is_empty());
    assert!(!h.pending_tool_providers.contains_key("running-call"));
    assert!(!h.pending_tool_providers.contains_key("queued-call"));

    let interrupted: ToolCallId = "running-call".into();
    let interrupted_message = extension_disconnected_tool_call_error_message(&interrupted);
    let unavailable_message = unavailable_tool_error_message(&ToolName::new("dead_slow"));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "running-call"
                && error.message == interrupted_message
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "queued-call"
                && error.message == unavailable_message
    )));

    h.shutdown().expect("shutdown");
}

fn final_tool_result(call_id: &str, tool_name: &str, text: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn wait_no_args_call(call_id: &str) -> AgentToolCall {
    AgentToolCall {
        id: call_id.into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
        display: None,
    }
}

fn tool_error(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolError {
    tau_proto::ToolError {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        message: message.to_owned(),
        details: None,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn tool_progress(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolProgress {
    tau_proto::ToolProgress {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        message: Some(message.to_owned()),
        progress: None,
    }
}

fn ext_query(query_id: &str, execution_mode: ToolExecutionMode) -> StartAgentRequest {
    StartAgentRequest {
        query_id: query_id.to_owned(),
        agent_id: format!("test-agent-{}", query_id),
        instruction: format!("instruction {query_id}"),
        role: None,
        execution_mode,
        input_stats: tau_proto::ToolDisplayStats::default(),
        tool_call_id: None,
        task_name: None,
    }
}

fn provider_model_info(
    id: tau_proto::ModelId,
    context_window: u64,
) -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id,
        display_name: None,
        default_affinity: 0,
        context_window,
        efforts: vec![tau_proto::Effort::Off, tau_proto::Effort::High],
        verbosities: vec![tau_proto::Verbosity::Low, tau_proto::Verbosity::High],
        thinking_summaries: vec![
            tau_proto::ThinkingSummary::Off,
            tau_proto::ThinkingSummary::Auto,
        ],
        supports_compaction: false,
    }
}

fn set_available_provider_models(
    h: &mut Harness,
    models: impl IntoIterator<Item = tau_proto::ProviderModelInfo>,
) {
    let models: Vec<_> = models.into_iter().collect();
    h.available_models = models.iter().map(|info| info.id.clone()).collect();
    h.provider_model_info = models
        .into_iter()
        .map(|info| (info.id.clone(), info))
        .collect();
}

fn ext_query_cid(h: &Harness, query_id: &str) -> Option<ConversationId> {
    h.conversations.iter().find_map(|(cid, conv)| {
        matches!(
            &conv.originator,
            tau_proto::PromptOriginator::Extension { query_id: id, .. } if id == query_id
        )
        .then_some(cid.clone())
    })
}

fn finish_ext_query(h: &mut Harness, cid: &ConversationId, query_id: &str) {
    let spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: format!("answer {query_id}"),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "test-ext".into(),
            query_id: query_id.to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish start-agent request");
}

/// Regression: a backgrounded update call still owns the serialized lane after
/// the synthetic placeholder closes its foreground. When the real background
/// result arrives, the harness must drain the queued tool scheduler so the next
/// update from the same model response can start.
#[test]
fn background_result_drains_queued_update_tool_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-bg-result-drain");
    h.registry.register(
        "conn-bg-result-drain",
        scheduled_test_tool_spec(
            "bg_update",
            ToolExecutionMode::Update,
            tau_proto::BackgroundSupport::Instant,
        ),
    );
    h.registry.register(
        "conn-bg-result-drain",
        scheduled_test_tool_spec(
            "queued_update",
            ToolExecutionMode::Update,
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-bg-result-drain".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-update-running".into(),
                name: ToolName::new("bg_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update".into(),
                name: ToolName::new("queued_update"),
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

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-update-running".to_owned()]
    );
    assert_eq!(background_placeholder_count(&h, "bg-update-running"), 1);
    assert!(h.tool_turn.is_backgrounded(&"bg-update-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 1);

    h.handle_extension_event_inner(
        "conn-bg-result-drain",
        Event::ToolResult(final_tool_result(
            "bg-update-running",
            "bg_update",
            "background output",
        )),
    )
    .expect("background result accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-update-running".to_owned(), "queued-update".to_owned()]
    );
    assert_eq!(background_result_count(&h, "bg-update-running"), 1);
    assert!(!h.tool_turn.is_backgrounded(&"bg-update-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(!h.pending_tool_providers.contains_key("bg-update-running"));
    assert!(h.pending_tool_providers.contains_key("queued-update"));

    h.shutdown().expect("shutdown");
}

/// Regression: background errors free the same actual-running scheduler lane as
/// background results. This covers an exclusive call blocking a queued update
/// so both serialized modes use the background-error drain path.
#[test]
fn background_error_drains_update_queued_behind_exclusive() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-bg-error-drain");
    h.registry.register(
        "conn-bg-error-drain",
        scheduled_test_tool_spec(
            "bg_exclusive",
            ToolExecutionMode::Exclusive,
            tau_proto::BackgroundSupport::Instant,
        ),
    );
    h.registry.register(
        "conn-bg-error-drain",
        scheduled_test_tool_spec(
            "queued_update_after_error",
            ToolExecutionMode::Update,
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-bg-error-drain".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-exclusive-running".into(),
                name: ToolName::new("bg_exclusive"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update-after-error".into(),
                name: ToolName::new("queued_update_after_error"),
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

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-exclusive-running".to_owned()]
    );
    assert_eq!(background_placeholder_count(&h, "bg-exclusive-running"), 1);
    assert!(h.tool_turn.is_backgrounded(&"bg-exclusive-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 1);

    h.handle_extension_event_inner(
        "conn-bg-error-drain",
        Event::ToolError(tool_error(
            "bg-exclusive-running",
            "bg_exclusive",
            "background failure",
        )),
    )
    .expect("background error accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-running".to_owned(),
            "queued-update-after-error".to_owned(),
        ]
    );
    assert_eq!(background_error_count(&h, "bg-exclusive-running"), 1);
    assert!(!h.tool_turn.is_backgrounded(&"bg-exclusive-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(
        !h.pending_tool_providers
            .contains_key("bg-exclusive-running")
    );
    assert!(
        h.pending_tool_providers
            .contains_key("queued-update-after-error")
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a cancelled backgrounded exclusive call frees its scheduler lane
/// without publishing a terminal background result. The cancellation path must
/// still drain queued calls so incompatible work behind it is not stuck until
/// an unrelated event arrives.
#[test]
fn background_cancel_drains_update_queued_behind_exclusive() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-bg-cancel-drain");
    h.registry.register(
        "conn-bg-cancel-drain",
        scheduled_test_tool_spec(
            "bg_exclusive_cancel",
            ToolExecutionMode::Exclusive,
            tau_proto::BackgroundSupport::Instant,
        ),
    );
    h.registry.register(
        "conn-bg-cancel-drain",
        scheduled_test_tool_spec(
            "queued_update_after_cancel",
            ToolExecutionMode::Update,
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-bg-cancel-drain".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-exclusive-cancel-running".into(),
                name: ToolName::new("bg_exclusive_cancel"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update-after-cancel".into(),
                name: ToolName::new("queued_update_after_cancel"),
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

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-exclusive-cancel-running".to_owned()]
    );
    assert_eq!(
        background_placeholder_count(&h, "bg-exclusive-cancel-running"),
        1
    );
    assert!(
        h.tool_turn
            .is_backgrounded(&"bg-exclusive-cancel-running".into())
    );
    assert_eq!(h.tool_turn.pending_len(), 1);

    h.handle_extension_event_inner(
        "conn-bg-cancel-drain",
        Event::ToolCancelled(tau_proto::ToolCancelled {
            call_id: "bg-exclusive-cancel-running".into(),
            tool_name: ToolName::new("bg_exclusive_cancel"),
            tool_type: tau_proto::ToolType::Function,
        }),
    )
    .expect("background cancellation accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-cancel-running".to_owned(),
            "queued-update-after-cancel".to_owned(),
        ]
    );
    assert!(
        !h.tool_turn
            .is_backgrounded(&"bg-exclusive-cancel-running".into())
    );
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(
        !h.pending_tool_providers
            .contains_key("bg-exclusive-cancel-running")
    );
    assert!(
        h.pending_tool_providers
            .contains_key("queued-update-after-cancel")
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled)
            if cancelled.call_id.as_str() == "bg-exclusive-cancel-running"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundResult(result)
            if result.call_id.as_str() == "bg-exclusive-cancel-running"
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: disconnect cleanup can synthesize errors for more than one
/// backgrounded call from the same dead provider. The queued scheduler must not
/// drain between those errors, or a newly-unblocked call can start before the
/// whole disconnect batch is visible to the conversation.
#[test]
fn disconnect_background_errors_drain_queued_tools_after_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let dead_events = connect_test_tool(&mut h, "conn-bg-disconnect-batch");
    let live_events = connect_test_tool(&mut h, "conn-bg-disconnect-live");
    h.registry.register(
        "conn-bg-disconnect-batch",
        scheduled_test_tool_spec(
            "dead_bg_shared",
            ToolExecutionMode::Shared,
            tau_proto::BackgroundSupport::Instant,
        ),
    );
    h.registry.register(
        "conn-bg-disconnect-batch",
        scheduled_test_tool_spec(
            "dead_bg_update",
            ToolExecutionMode::Update,
            tau_proto::BackgroundSupport::Instant,
        ),
    );
    h.registry.register(
        "conn-bg-disconnect-live",
        scheduled_test_tool_spec(
            "live_queued_update",
            ToolExecutionMode::Update,
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-bg-disconnect-batch".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-bg-shared".into(),
                name: ToolName::new("dead_bg_shared"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-bg-update".into(),
                name: ToolName::new("dead_bg_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "z-queued-update".into(),
                name: ToolName::new("live_queued_update"),
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

    assert_eq!(
        tool_invoke_call_ids(&dead_events),
        vec!["b-bg-shared".to_owned(), "a-bg-update".to_owned()]
    );
    assert!(tool_invoke_call_ids(&live_events).is_empty());
    assert_eq!(h.tool_turn.pending_len(), 1);
    assert!(h.tool_turn.is_backgrounded(&"a-bg-update".into()));
    assert!(h.tool_turn.is_backgrounded(&"b-bg-shared".into()));

    h.handle_disconnect("conn-bg-disconnect-batch");

    assert_eq!(
        tool_invoke_call_ids(&live_events),
        vec!["z-queued-update".to_owned()]
    );
    assert_eq!(background_error_count(&h, "a-bg-update"), 1);
    assert_eq!(background_error_count(&h, "b-bg-shared"), 1);
    assert!(!h.pending_tool_providers.contains_key("a-bg-update"));
    assert!(!h.pending_tool_providers.contains_key("b-bg-shared"));
    assert_eq!(
        h.pending_tool_providers
            .get("z-queued-update")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-bg-disconnect-live")
    );

    let update_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "a-bg-update"
        )
    })
    .expect("update background error");
    let shared_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "b-bg-shared"
        )
    })
    .expect("shared background error");
    let queued_request_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolRequest(request) if request.call_id.as_str() == "z-queued-update"
        )
    })
    .expect("queued request");
    assert!(update_error_seq < queued_request_seq);
    assert!(shared_error_seq < queued_request_seq);

    h.shutdown().expect("shutdown");
}

/// Regression: when an idle conversation has more than one backgrounded call on
/// a disconnected provider, the harness must record every synthetic background
/// error before it dispatches the first internal completion prompt back to the
/// model. Dispatching after the first error would let the follow-up miss later
/// failures from the same disconnect batch.
#[test]
fn disconnect_idle_multi_background_errors_dispatch_prompt_after_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _dead_events = connect_test_tool(&mut h, "conn-bg-idle-disconnect");
    h.registry.register(
        "conn-bg-idle-disconnect",
        scheduled_test_tool_spec(
            "dead_bg_one",
            ToolExecutionMode::Shared,
            tau_proto::BackgroundSupport::Instant,
        ),
    );
    h.registry.register(
        "conn-bg-idle-disconnect",
        scheduled_test_tool_spec(
            "dead_bg_two",
            ToolExecutionMode::Shared,
            tau_proto::BackgroundSupport::Instant,
        ),
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-bg-idle-disconnect".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-bg-idle".into(),
                name: ToolName::new("dead_bg_one"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-bg-idle".into(),
                name: ToolName::new("dead_bg_two"),
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
    .expect("background tool response");

    let followup_spid = match &h
        .conversations
        .get(&cid)
        .expect("conversation remains live")
        .turn_state
    {
        ConversationTurnState::AgentThinking { session_prompt_id } => session_prompt_id.clone(),
        state => panic!("expected placeholder follow-up prompt, got {state:?}"),
    };
    h.handle_provider_response_finished(provider_text_response(
        &followup_spid,
        "placeholders observed",
    ))
    .expect("finish placeholder follow-up");
    assert!(matches!(
        h.conversations
            .get(&cid)
            .expect("conversation remains live")
            .turn_state,
        ConversationTurnState::Idle
    ));

    h.handle_disconnect("conn-bg-idle-disconnect");

    let first_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "a-bg-idle"
        )
    })
    .expect("first background error");
    let second_error_seq = event_log_position_after(&h, first_error_seq, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "b-bg-idle"
        )
    })
    .expect("second background error");
    let prompt_after_first_error_seq = event_log_position_after(&h, first_error_seq, |event| {
        matches!(event, Event::SessionPromptCreated(_))
    })
    .expect("background completion follow-up prompt");
    assert!(second_error_seq < prompt_after_first_error_seq);

    h.shutdown().expect("shutdown");
}

/// Regression: a disconnect batch can contain a foreground call that completes
/// the model's tool round plus a later background error. The foreground failure
/// must not complete the agent turn and dispatch a follow-up until the
/// background error from the same dead provider has also been recorded.
#[test]
fn disconnect_mixed_foreground_and_background_errors_dispatch_prompt_after_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let dead_events = connect_test_tool(&mut h, "conn-mixed-disconnect");
    h.registry.register(
        "conn-mixed-disconnect",
        scheduled_test_tool_spec(
            "dead_foreground",
            ToolExecutionMode::Shared,
            tau_proto::BackgroundSupport::Never,
        ),
    );
    h.registry.register(
        "conn-mixed-disconnect",
        scheduled_test_tool_spec(
            "dead_background",
            ToolExecutionMode::Shared,
            tau_proto::BackgroundSupport::Instant,
        ),
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-mixed-disconnect".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-foreground-disconnect".into(),
                name: ToolName::new("dead_foreground"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-background-disconnect".into(),
                name: ToolName::new("dead_background"),
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
    .expect("mixed tool response");

    assert_eq!(
        tool_invoke_call_ids(&dead_events),
        vec![
            "a-foreground-disconnect".to_owned(),
            "b-background-disconnect".to_owned(),
        ]
    );
    assert!(
        h.tool_turn
            .is_backgrounded(&"b-background-disconnect".into())
    );
    assert!(matches!(
        h.conversations
            .get(&cid)
            .expect("conversation remains live")
            .turn_state,
        ConversationTurnState::ToolsRunning { .. }
    ));

    h.handle_disconnect("conn-mixed-disconnect");

    let foreground_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolError(error) if error.call_id.as_str() == "a-foreground-disconnect"
        )
    })
    .expect("foreground synthetic error");
    let background_error_seq = event_log_position_after(&h, foreground_error_seq, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "b-background-disconnect"
        )
    })
    .expect("background synthetic error");
    let prompt_after_foreground_error_seq =
        event_log_position_after(&h, foreground_error_seq, |event| {
            matches!(event, Event::SessionPromptCreated(_))
        })
        .expect("post-disconnect follow-up prompt");
    assert!(background_error_seq < prompt_after_foreground_error_seq);

    h.shutdown().expect("shutdown");
}

/// A tool result from any connection other than the routed provider must not
/// close the call; otherwise a stale extension can spoof completion and make
/// the real owner look like a duplicate.
#[test]
fn provider_owner_validation_rejects_wrong_tool_result() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-result-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolResult(final_tool_result(
            "owner-result-call",
            "owned_tool",
            "spoofed output",
        )),
    )
    .expect("wrong result ignored");

    assert!(h.tool_conversations.contains_key("owner-result-call"));
    assert_eq!(
        h.pending_tool_providers
            .get("owner-result-call")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolResult(result) if result.call_id.as_str() == "owner-result-call"
    )));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolResult(final_tool_result(
            "owner-result-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");

    assert!(!h.tool_conversations.contains_key("owner-result-call"));
    assert!(!h.pending_tool_providers.contains_key("owner-result-call"));
    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "owner-result-call"
                && matches!(&result.result, CborValue::Text(text) if text == "real output")
    )));

    h.shutdown().expect("shutdown");
}

/// A tool error from a non-owner is also ignored so it cannot fail the pending
/// call or remove routing state before the owner reports the real failure.
#[test]
fn provider_owner_validation_rejects_wrong_tool_error() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-error-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolError(tool_error(
            "owner-error-call",
            "owned_tool",
            "spoofed failure",
        )),
    )
    .expect("wrong error ignored");

    assert!(h.tool_conversations.contains_key("owner-error-call"));
    assert_eq!(
        h.pending_tool_providers
            .get("owner-error-call")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolError(error) if error.call_id.as_str() == "owner-error-call"
    )));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolError(tool_error("owner-error-call", "owned_tool", "real failure")),
    )
    .expect("owner error accepted");

    assert!(!h.tool_conversations.contains_key("owner-error-call"));
    assert!(!h.pending_tool_providers.contains_key("owner-error-call"));
    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "owner-error-call" && error.message == "real failure"
    )));

    h.shutdown().expect("shutdown");
}

/// Progress is non-terminal, but it still must come from the routed owner so a
/// wrong extension cannot publish spoofed output into the visible tool block.
#[test]
fn provider_owner_validation_rejects_wrong_tool_progress() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-progress-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolProgress(tool_progress(
            "owner-progress-call",
            "owned_tool",
            "spoofed progress",
        )),
    )
    .expect("wrong progress ignored");

    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolProgress(progress) if progress.call_id.as_str() == "owner-progress-call"
    )));
    assert!(h.tool_conversations.contains_key("owner-progress-call"));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolProgress(tool_progress(
            "owner-progress-call",
            "owned_tool",
            "real progress",
        )),
    )
    .expect("owner progress accepted");

    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolProgress(progress)
            if progress.call_id.as_str() == "owner-progress-call"
                && progress.message.as_deref() == Some("real progress")
    )));

    h.shutdown().expect("shutdown");
}

/// A terminal cancellation from a non-owner must not poison the tool round
/// before the routed provider returns the real result.
#[test]
fn provider_owner_validation_rejects_wrong_tool_cancelled() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-cancelled-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolCancelled(tau_proto::ToolCancelled {
            call_id: "owner-cancelled-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
        }),
    )
    .expect("wrong cancellation ignored");

    assert!(h.tool_conversations.contains_key("owner-cancelled-call"));
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "owner-cancelled-call"
    )));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolResult(final_tool_result(
            "owner-cancelled-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");

    assert!(!h.tool_conversations.contains_key("owner-cancelled-call"));
    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "owner-cancelled-call"
                && matches!(&result.result, CborValue::Text(text) if text == "real output")
    )));

    h.shutdown().expect("shutdown");
}

/// Background terminal events are harness-derived records. Extensions must not
/// be able to inject them directly into the session log.
#[test]
fn provider_owner_validation_rejects_external_background_result() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-background-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
            call_id: "owner-background-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("spoofed background".to_owned()),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("wrong background result ignored");

    assert!(h.tool_conversations.contains_key("owner-background-call"));
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolBackgroundResult(result) if result.call_id.as_str() == "owner-background-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_external_background_error() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-background-error-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "owner-background-error-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
            message: "spoofed background error".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("wrong background error ignored");

    assert!(
        h.tool_conversations
            .contains_key("owner-background-error-call")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "owner-background-error-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_external_provider_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let _provider = connect_test_client(&mut h, "provider-spoof", tau_proto::ClientKind::Provider);

    h.handle_extension_event_inner(
        "provider-spoof",
        Event::ProviderToolResult(final_tool_result(
            "provider-tool-call",
            "owned_tool",
            "spoofed provider result",
        )),
    )
    .expect("provider tool result ignored");

    assert!(!event_log_contains(&h, "provider-spoof", |event| matches!(
        event,
        Event::ProviderToolResult(result) if result.call_id.as_str() == "provider-tool-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_tool_event_message_emit() {
    let (_td, mut h) = setup_routed_test_tool_call("emit-cancelled-call", "owned_tool");

    h.handle_extension_event(
        "conn-wrong",
        Frame::Message(Message::Emit(tau_proto::Emit {
            event: Box::new(Event::ToolCancelled(tau_proto::ToolCancelled {
                call_id: "emit-cancelled-call".into(),
                tool_name: ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
            })),
            transient: false,
        })),
    )
    .expect("emitted cancellation ignored");

    assert!(h.tool_conversations.contains_key("emit-cancelled-call"));
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "emit-cancelled-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_provider_event_message_emit() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.handle_extension_event(
        "conn-wrong",
        Frame::Message(Message::Emit(tau_proto::Emit {
            event: Box::new(Event::ProviderResponseFinished(ProviderResponseFinished {
                session_prompt_id: "spoofed-prompt".into(),
                output_items: Vec::new(),
                stop_reason: tau_proto::ProviderStopReason::EndTurn,
                usage: None,
                originator: tau_proto::PromptOriginator::User,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            })),
            transient: false,
        })),
    )
    .expect("emitted provider event ignored");

    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.session_prompt_id.as_str() == "spoofed-prompt"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_late_tool_progress_after_completion() {
    let (_td, mut h) = setup_routed_test_tool_call("late-progress-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolResult(final_tool_result(
            "late-progress-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");
    assert!(!h.tool_conversations.contains_key("late-progress-call"));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolProgress(tool_progress(
            "late-progress-call",
            "owned_tool",
            "late progress",
        )),
    )
    .expect("late progress ignored");

    assert!(!event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolProgress(progress)
            if progress.call_id.as_str() == "late-progress-call"
                && progress.message.as_deref() == Some("late progress")
    )));

    h.shutdown().expect("shutdown");
}

/// Cancelling a routed tool publishes the durable broadcast cancellation
/// request and the local terminal `ToolCancelled` event. Extensions observe the
/// event log instead of receiving point-to-point cancellation frames.
#[test]
fn cancel_publishes_tool_cancel_request() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _owner_events = connect_test_tool(&mut h, "conn-cancel-owner");
    h.registry
        .register("conn-cancel-owner", shared_test_tool_spec("cancel_tool"));

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-cancel-tool".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "cancel-call".into(),
            name: ToolName::new("cancel_tool"),
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
    .expect("tool call routed");

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: None,
        session_prompt_id: None,
    });

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id.as_str() == "cancel-call"
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "cancel-call"
    )));

    h.shutdown().expect("shutdown");
}

/// Cancelling a turn while `wait` is blocked must remove the waiter entry. A
/// later wait for the same target should report the cancelled/consumed target,
/// not a stale "existing wait" from the aborted wait call.
#[test]
fn cancel_clears_active_wait_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let target_call_id: ToolCallId = "wait-target".into();
    let wait_call_id: ToolCallId = "wait-call".into();

    h.tool_conversations
        .insert(target_call_id.clone(), cid.clone());
    h.pending_tools.insert(
        target_call_id.clone(),
        PendingTool {
            name: ToolName::new("slow"),
            internal_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.record_wait_tool_request(&target_call_id);

    let wait_call = AgentToolCall {
        id: wait_call_id.clone(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
        display: None,
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");
    seed_tools_running(
        &mut h,
        &cid,
        vec![target_call_id.clone(), wait_call_id.clone()],
    );

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: None,
        session_prompt_id: None,
    });

    let second_wait_call = AgentToolCall {
        id: "wait-call-2".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
        display: None,
    };
    h.handle_wait_tool_call(&cid, &second_wait_call, ToolName::new("wait"))
        .expect("second wait");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-call-2"
                && error.message.contains("already consumed")
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn cross_session_prompt_is_rejected() {
    // The harness owns one session at a time. A UserMessage with
    // a different session id must not silently spin up a second
    // session — it gets rejected with a clear reason.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"

    h.selected_model = Some("test/model".into());
    let submission = h
        .submit_user_prompt("chat-1".into(), "hello".to_owned())
        .expect("submit");
    match submission {
        PromptSubmission::Rejected { reason } => {
            assert!(reason.contains("s1"), "reason should name bound session");
            assert!(reason.contains("chat-1"), "reason should name rejected id");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(
        h.conversations
            .get(&h.default_conversation_id)
            .expect("default conversation")
            .pending_prompts
            .is_empty(),
        "rejected prompt must not queue"
    );
    assert!(
        h.store.session("chat-1").is_none(),
        "rejected session must not be created"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_model_prompt_routes_directly_to_provider_owner() {
    // Provider-published models should not wake every provider subscriber.
    // The committed prompt remains visible to observers, while the owner gets a
    // direct LogEvent even without subscribing to session.prompt_created.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let provider_frames =
        connect_test_client(&mut h, "provider-owner", tau_proto::ClientKind::Provider);
    let provider_observer_frames =
        connect_test_client(&mut h, "provider-observer", tau_proto::ClientKind::Provider);
    let ui_frames = connect_test_client(&mut h, "ui-observer", tau_proto::ClientKind::Ui);
    let prompt_selector = vec![EventSelector::Exact(
        tau_proto::EventName::SESSION_PROMPT_CREATED,
    )];
    h.bus
        .set_subscriptions("provider-observer", prompt_selector.clone())
        .expect("provider observer subscription");
    h.bus
        .set_subscriptions("ui-observer", prompt_selector)
        .expect("ui observer subscription");

    let model_id: tau_proto::ModelId = "openai/gpt-5.5".parse().expect("model id");
    h.handle_extension_event(
        "provider-owner",
        Frame::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![tau_proto::ProviderModelInfo {
                    id: model_id.clone(),
                    display_name: None,
                    default_affinity: 0,
                    context_window: 200_000,
                    efforts: vec![tau_proto::Effort::Medium],
                    verbosities: vec![tau_proto::Verbosity::Medium],
                    thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                    supports_compaction: false,
                }],
            },
        )),
    )
    .expect("provider model snapshot");
    h.selected_model = Some(model_id);

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid = h.send_prompt_to_agent("s1");

    let frame_is_prompt = |routed: &RoutedFrame, spid: &SessionPromptId| {
        let (_, inner) = routed.frame.clone().peel_log();
        matches!(
            inner,
            Frame::Event(Event::SessionPromptCreated(prompt))
                if prompt.session_prompt_id.as_str() == spid.as_str()
        )
    };
    assert!(
        provider_frames
            .lock()
            .expect("provider frames")
            .iter()
            .any(|routed| frame_is_prompt(routed, &spid)),
        "provider owner should receive the direct prompt request"
    );
    assert!(
        ui_frames
            .lock()
            .expect("ui frames")
            .iter()
            .any(|routed| frame_is_prompt(routed, &spid)),
        "UI observer should still see the committed prompt fact"
    );
    assert!(
        provider_observer_frames
            .lock()
            .expect("provider observer frames")
            .is_empty(),
        "provider observers should not receive provider-owned prompt execution"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_execution_events_must_come_from_prompt_owner() {
    // Provider execution is point-to-point. Once the harness routes a prompt to
    // the provider that published the selected model, streaming and final
    // response events for that prompt must come back from the same connection.
    // Otherwise a second provider participant could spoof a response for an
    // in-flight prompt it never received.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let _owner_frames =
        connect_test_client(&mut h, "provider-owner", tau_proto::ClientKind::Provider);
    let _other_frames =
        connect_test_client(&mut h, "provider-other", tau_proto::ClientKind::Provider);
    let _tool_frames =
        connect_test_client(&mut h, "tool-impersonator", tau_proto::ClientKind::Tool);

    let model_id: tau_proto::ModelId = "openai/gpt-5.5".parse().expect("model id");
    h.handle_extension_event(
        "provider-owner",
        Frame::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![tau_proto::ProviderModelInfo {
                    id: model_id.clone(),
                    display_name: None,
                    default_affinity: 0,
                    context_window: 200_000,
                    efforts: vec![tau_proto::Effort::Medium],
                    verbosities: vec![tau_proto::Verbosity::Medium],
                    thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                    supports_compaction: false,
                }],
            },
        )),
    )
    .expect("provider model snapshot");
    h.selected_model = Some(model_id);

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid = h.send_prompt_to_agent("s1");
    assert_eq!(
        h.pending_provider_prompts.get(&spid).map(|id| id.as_str()),
        Some("provider-owner"),
        "outbound prompt owner should be recorded"
    );

    h.handle_extension_event(
        "provider-other",
        Frame::Event(Event::ProviderResponseUpdated(ProviderResponseUpdated {
            session_prompt_id: spid.clone(),
            text: "spoofed stream".to_owned(),
            thinking: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("forged stream from provider");
    h.handle_extension_event(
        "tool-impersonator",
        Frame::Event(Event::ProviderResponseUpdated(ProviderResponseUpdated {
            session_prompt_id: spid.clone(),
            text: "tool stream".to_owned(),
            thinking: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("forged stream from tool");
    h.handle_extension_event(
        "provider-other",
        Frame::Event(Event::ProviderResponseFinished(provider_text_response(
            &spid,
            "spoofed final",
        ))),
    )
    .expect("forged final response");

    assert_eq!(
        h.pending_provider_prompts.get(&spid).map(|id| id.as_str()),
        Some("provider-owner"),
        "wrong-source events must not consume the pending owner"
    );
    assert!(matches!(
        h.conversations[&h.default_conversation_id].turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));
    assert!(!event_log_contains(&h, "provider-other", |event| matches!(
        event,
        Event::ProviderResponseUpdated(_) | Event::ProviderResponseFinished(_)
    )));
    assert!(!event_log_contains(
        &h,
        "tool-impersonator",
        |event| matches!(
            event,
            Event::ProviderResponseUpdated(_) | Event::ProviderResponseFinished(_)
        )
    ));

    h.handle_extension_event(
        "provider-owner",
        Frame::Event(Event::ProviderResponseUpdated(ProviderResponseUpdated {
            session_prompt_id: spid.clone(),
            text: "real stream".to_owned(),
            thinking: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("owner stream");
    h.handle_extension_event(
        "provider-owner",
        Frame::Event(Event::ProviderResponseFinished(provider_text_response(
            &spid,
            "real final",
        ))),
    )
    .expect("owner final response");

    assert!(!h.pending_provider_prompts.contains_key(&spid));
    assert!(matches!(
        h.conversations[&h.default_conversation_id].turn_state,
        ConversationTurnState::Idle
    ));
    assert!(event_log_contains(&h, "provider-owner", |event| matches!(
        event,
        Event::ProviderResponseUpdated(_)
    )));
    assert!(event_log_contains(&h, "provider-owner", |event| matches!(
        event,
        Event::ProviderResponseFinished(_)
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn shared_exclusive_shared_serializes_through_dispatch_state_machine() {
    use tau_proto::ToolExecutionMode::{Exclusive, Shared};

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed turn state as if the agent had just been prompted
    // and is about to respond with tool calls.
    h.selected_model = Some("test/model".into());
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid);

    // A `read` of a nonexistent path returns a ToolError (Shared);
    // `write` of a valid path creates the file and returns
    // ToolResult (Exclusive). Either kind of response path is
    // handled identically by the state machine.
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text("/nonexistent/tau-test-path".to_owned()),
    )]);
    let write_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("w.txt").display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text("hi".to_owned()),
        ),
    ]);
    let response = ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: read_args.clone(),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: tau_proto::ToolName::new("write"),
                tool_type: tau_proto::ToolType::Function,
                arguments: write_args,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c3".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: read_args,
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

    h.handle_provider_response_finished(response)
        .expect("finished");

    // Right after dispatch, only c1 (Shared) should be in-flight;
    // c2 (Exclusive) and c3 (Shared behind the Exclusive) must wait.
    let c1_id: ToolCallId = "c1".to_owned().into();
    let c2_id: ToolCallId = "c2".to_owned().into();
    let c3_id: ToolCallId = "c3".to_owned().into();
    assert_eq!(h.tool_turn.in_flight_len(), 1);
    assert_eq!(h.tool_turn.in_flight_mode(&c1_id), Some(&Shared));
    assert_eq!(h.tool_turn.pending_len(), 2);
    assert_eq!(
        h.tool_turn
            .pending(0)
            .expect("c2 should be queued")
            .invocation
            .id,
        "c2"
    );
    assert_eq!(
        h.tool_turn
            .pending(1)
            .expect("c3 should be queued")
            .invocation
            .id,
        "c3"
    );

    drive_harness_until_call_completes(&mut h, "c1");

    // After c1 completes the Exclusive gate opens and c2 dispatches.
    // c3 must stay queued behind it.
    assert_eq!(h.tool_turn.in_flight_len(), 1);
    assert_eq!(h.tool_turn.in_flight_mode(&c2_id), Some(&Exclusive));
    assert_eq!(h.tool_turn.pending_len(), 1);
    assert_eq!(
        h.tool_turn
            .pending(0)
            .expect("c3 should stay queued")
            .invocation
            .id,
        "c3"
    );

    drive_harness_until_call_completes(&mut h, "c2");

    // With the Exclusive cleared, c3 finally dispatches.
    assert_eq!(h.tool_turn.in_flight_len(), 1);
    assert_eq!(h.tool_turn.in_flight_mode(&c3_id), Some(&Shared));
    assert_eq!(h.tool_turn.pending_len(), 0);

    drive_harness_until_call_completes(&mut h, "c3");
    assert!(h.tool_turn.is_empty());

    h.shutdown().expect("shutdown");
}

#[test]
fn multi_tool_turn_keeps_all_results_in_followup_prompt() {
    // Regression: when several tool calls complete in sequence, every
    // ToolResult must end up on the current branch so the follow-up
    // prompt sees a balanced tool_use ↔ tool_result set. A previous
    // bug let `publish_event` (used by the ToolResult/ToolError path)
    // leave the conversation's local head stale, so the next
    // ToolRequest's `publish_for_conversation` emitted a
    // `UiNavigateTree` that bounced the tree head backward — orphaning
    // the just-published ToolResult onto a dead branch and triggering
    // OpenAI's "No tool output found for function call ..." 400.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "go");
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid);

    let write_args = |name: &str| {
        CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(td.path().join(name).display().to_string()),
            ),
            (
                CborValue::Text("content".to_owned()),
                CborValue::Text(name.to_owned()),
            ),
        ])
    };
    let response = ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: tau_proto::ToolName::new("write"),
                tool_type: tau_proto::ToolType::Function,
                arguments: write_args("a.txt"),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: tau_proto::ToolName::new("write"),
                tool_type: tau_proto::ToolType::Function,
                arguments: write_args("b.txt"),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c3".into(),
                name: tau_proto::ToolName::new("write"),
                tool_type: tau_proto::ToolType::Function,
                arguments: write_args("c.txt"),
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
    h.handle_provider_response_finished(response)
        .expect("finished");

    drive_harness_until_call_completes(&mut h, "c1");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "c1"
                && result.kind == tau_proto::ToolResultKind::Final
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "c1"
                && result.kind == tau_proto::ToolResultKind::Final
    )));
    drive_harness_until_call_completes(&mut h, "c2");
    drive_harness_until_call_completes(&mut h, "c3");

    // After all three tools complete, the harness has auto-dispatched
    // a follow-up prompt. Read its context items and check that every
    // tool call has a matching tool result on the same branch.
    let spid: SessionPromptId = "sp-0".into();
    let prompt = read_prompt_created(&h, &spid);
    let tool_use_ids: Vec<String> = prompt
        .context_items
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    let tool_result_ids: Vec<String> = prompt
        .context_items
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert_eq!(
        tool_use_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "follow-up prompt must keep every tool_use; got {tool_use_ids:?}"
    );
    assert_eq!(
        tool_result_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "every tool_use must be paired with a tool_result on the current branch; \
         got {tool_result_ids:?}"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_prompt_is_steered_into_next_round_after_tool_result() {
    // While the agent is mid-turn (a tool is in flight), a fresh user
    // prompt must queue rather than dispatch. When the tool result
    // arrives and the harness is about to issue the next-round prompt,
    // it should drain the queued prompt onto this conversation's
    // branch as a `SessionPromptSteered` event so it rides the same
    // `SessionPromptCreated` as the tool results — instead of waiting
    // for full `Idle` and starting a separate turn.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());

    let write_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("a.txt").display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text("a".to_owned()),
        ),
    ]);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: tau_proto::ToolName::new("write"),
            tool_type: tau_proto::ToolType::Function,
            arguments: write_args,
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
    .expect("agent response with tool call");

    // The conversation must be in `ToolsRunning` so `submit_user_prompt`
    // takes the queued path rather than dispatching.
    assert!(matches!(
        h.conversations.get(&cid).expect("default").turn_state,
        ConversationTurnState::ToolsRunning { .. }
    ));

    let submission = h
        .submit_user_prompt("s1".into(), "redirect".to_owned())
        .expect("submit");
    assert!(
        matches!(submission, PromptSubmission::Queued),
        "in-flight turn should force queueing, got {submission:?}"
    );
    assert_eq!(
        h.conversations
            .get(&cid)
            .expect("default")
            .pending_prompts
            .len(),
        1,
        "the steering message should sit in pending_prompts until the next-round seam",
    );

    drive_harness_until_call_completes(&mut h, "c1");

    assert!(
        h.conversations
            .get(&cid)
            .expect("default")
            .pending_prompts
            .is_empty(),
        "queued prompt must be drained when folded as a steer",
    );

    // Walk the event log and verify ordering: the SessionPromptSteered
    // is published before the next-round SessionPromptCreated, and the
    // latter's `context_items` includes the steered text alongside the
    // original user prompt.
    let next_round_spid: SessionPromptId = "sp-0".into();
    let mut cursor = 0;
    let mut saw_steered = false;
    let mut saw_next_round = false;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        match &entry.event {
            Event::SessionPromptSteered(steered) => {
                assert_eq!(steered.text, "redirect");
                assert!(
                    !saw_next_round,
                    "steered event must precede the prompt it folds into",
                );
                saw_steered = true;
            }
            Event::SessionPromptCreated(p) if p.session_prompt_id == next_round_spid => {
                assert!(
                    saw_steered,
                    "next-round prompt must follow the SessionPromptSteered",
                );
                saw_next_round = true;

                let user_texts: Vec<String> = p
                    .context_items
                    .iter()
                    .filter_map(|item| match item {
                        ContextItem::Message(MessageItem {
                            role: ContextRole::User,
                            ..
                        }) => text_part(item).map(str::to_owned),
                        _ => None,
                    })
                    .collect();
                assert!(
                    user_texts.iter().any(|t| t == "redirect"),
                    "next-round prompt should fold the steered message into messages; \
                     user texts were {user_texts:?}",
                );

                // The steered message must land *after* the tool result
                // on the same branch — otherwise the model sees its
                // tool_use replied to with a steer instead of the
                // ToolResult, which providers reject.
                let last_tool_result_idx = p
                    .context_items
                    .iter()
                    .rposition(|item| matches!(item, ContextItem::ToolResult(_)));
                let last_user_idx = p.context_items.iter().rposition(|item| {
                    matches!(
                        item,
                        ContextItem::Message(MessageItem {
                            role: ContextRole::User,
                            ..
                        }) if text_part(item) == Some("redirect")
                    )
                });
                assert!(
                    last_tool_result_idx.is_some(),
                    "next-round prompt must include the tool result"
                );
                assert!(
                    matches!((last_tool_result_idx, last_user_idx),
                        (Some(t), Some(u)) if u > t),
                    "steered user message must follow the tool result, not precede it",
                );
            }
            _ => {}
        }
    }
    assert!(saw_steered, "expected a SessionPromptSteered event");
    assert!(
        saw_next_round,
        "expected the next-round SessionPromptCreated"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn tool_calls_stop_reason_without_tool_items_does_not_wedge_turn() {
    // Providers can disagree between their terminal stop reason and
    // emitted item list. With no concrete tool-call items, there is no
    // round Tau can execute, so the harness must finish this model call
    // instead of entering an empty ToolsRunning state.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "hello".to_owned())
        .expect("submit");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-0".into(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "done".to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish");

    let cid = h.default_conversation_id.clone();
    assert!(matches!(
        h.conversations.get(&cid).expect("default").turn_state,
        ConversationTurnState::Idle
    ));
    assert_eq!(h.tool_turn.pending_len(), 0);

    h.submit_user_prompt("s1".into(), "again".to_owned())
        .expect("submit again");
    assert!(matches!(
        h.conversations.get(&cid).expect("default").turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn session_prompt_created_uses_refs_for_linear_extension() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "hi".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
    .expect("finish first");

    append_user_message_via_event(&mut h, "s1", "again");
    let spid2 = h.send_prompt_to_agent("s1");
    let raw2 = read_raw_prompt_created(&h, &spid2);
    let prompt2 = read_prompt_created(&h, &spid2);
    assert!(raw2.tools_ref.is_none());
    assert_eq!(raw2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(raw2.context_items, prompt2.context_items);
    assert_eq!(prompt2.tools, prompt1.tools);

    h.shutdown().expect("shutdown");
}

#[test]
fn linear_session_prompts_strictly_extend_previous_messages() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "hello");

    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "hi".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
    .expect("persist first agent response");

    append_user_message_via_event(&mut h, "s1", "again");

    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);

    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.tools, prompt1.tools);
    assert_eq!(prompt2.model, prompt1.model);
    assert_eq!(prompt2.model_params, prompt1.model_params);
    assert!(
        prompt1.context_items.len() < prompt2.context_items.len(),
        "second prompt should strictly extend first: {} !< {}",
        prompt1.context_items.len(),
        prompt2.context_items.len()
    );
    assert_eq!(
        &prompt2.context_items[..prompt1.context_items.len()],
        prompt1.context_items.as_slice(),
        "second prompt must keep first prompt context items as an exact prefix"
    );

    h.shutdown().expect("shutdown");
}

/// When the agent reports a `response_id` on a finished turn, the
/// next `SessionPromptCreated` for that conversation must carry a
/// `previous_response_candidate` pointing back at it — that's the hook the
/// Responses backend uses to switch into stateful-chain mode and
/// send just the delta upstream. `next_item_index` must equal the
/// assembled item count at the moment the anchor was captured,
/// so the delta slice is exactly the items added since.
#[test]
fn response_id_anchors_next_prompt_with_previous_response() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    let prev = prompt2
        .previous_response_candidate
        .expect("chain anchor on prompt 2");
    assert_eq!(prev.provider_response_id, "resp_abc");
    // After turn 1 finished and was folded, the assembled count is:
    //   user "first" + assistant "first answer" = 2 context items.
    // That's the slice point — `context_items[2..]` on prompt 2 is
    // just the new "second" user turn (1 item).
    assert_eq!(prev.next_item_index, prompt1.context_items.len() + 1);
    assert_eq!(prev.next_item_index + 1, prompt2.context_items.len());

    h.shutdown().expect("shutdown");
}

#[test]
fn chained_low_corrected_cache_hit_emits_diagnostic() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: match (Some(1_000), Some(0), None) {
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
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid2.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "second answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: match (Some(1_100), Some(0), None) {
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
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_def".to_owned()),
        ws_pool_delta: Some(tau_proto::WsPoolDelta {
            upgrades: 0,
            silent_reconnects: 0,
            chain_strips_on_fresh: 0,
        }),
    })
    .expect("finish second");

    let mut cursor = 0;
    let mut diagnostic = None;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        if let Event::ProviderCacheMissDiagnostic(event) = entry.event {
            diagnostic = Some(event);
        }
    }
    let diagnostic = diagnostic.expect("cache miss diagnostic");
    assert_eq!(diagnostic.session_prompt_id, spid2);
    assert_eq!(diagnostic.model, Some("test/model".into()));
    assert_eq!(diagnostic.previous_response_id, "resp_abc");
    assert_eq!(diagnostic.input_tokens, 1_100);
    assert_eq!(diagnostic.cached_tokens, 0);
    assert_eq!(diagnostic.previous_input_tokens, 1_000);
    assert_eq!(diagnostic.cacheable_input_tokens, 512);
    assert_eq!(diagnostic.corrected_cache_efficiency, 0.0);
    assert_eq!(diagnostic.request_body_fingerprint.len(), 64);

    h.shutdown().expect("shutdown");
}

#[test]
fn chained_sub_chunk_cacheable_tokens_does_not_emit_diagnostic() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: match (Some(500), Some(0), None) {
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
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid2,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "second answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: match (Some(500), Some(0), None) {
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
        provider_response_id: Some("resp_def".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish second");

    let mut cursor = 0;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        assert!(
            !matches!(entry.event, Event::ProviderCacheMissDiagnostic(_)),
            "sub-cache-chunk turn must not emit cache miss diagnostic"
        );
    }

    h.shutdown().expect("shutdown");
}

/// Switching `selected_model` mid-conversation must bust the chain.
/// The prior response was produced by a different model — its
/// stored state on the upstream API is meaningless for the new
/// model, and sending `previous_response_id` would either error or
/// silently mix incompatible reasoning.
#[test]
fn model_switch_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model-a".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // The selected role resolves to a different model.
    h.selected_model = Some("test/model-b".into());

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    assert!(
        prompt2.previous_response_candidate.is_none(),
        "resolved model change must clear the previous-response anchor"
    );

    h.shutdown().expect("shutdown");
}

/// Changing role-derived model parameters mid-conversation must bust the chain.
/// The Codex Responses upstream stored its reasoning state against
/// the *previous* turn's effort/verbosity/thinking-summary; sending
/// a `previous_response_id` from a request whose non-input fields
/// drifted would silently decohere the model's reasoning. The
/// fingerprint check catches this before the round-trip — mirrors
/// Pi's `requestBodiesMatchExceptInput`.
#[test]
fn params_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    h.available_roles
        .get_mut(&h.selected_role.clone())
        .expect("selected role")
        .effort = Some(tau_proto::Effort::Low);

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // User dials effort up between turns by updating the selected role override.
    h.available_roles
        .get_mut(&h.selected_role.clone())
        .expect("selected role")
        .effort = Some(tau_proto::Effort::High);

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    assert!(
        prompt2.previous_response_candidate.is_none(),
        "params drift must clear the previous-response anchor"
    );
}

/// A skill loading mid-conversation (and surfacing into the system
/// prompt) must also bust the chain — the upstream stored its
/// reasoning state against the *previous* system prompt, and
/// chaining a request whose `instructions` field has new content
/// would silently mix the skill's guidance with reasoning that
/// never saw it. This is the more likely real-world trigger for a
/// fingerprint miss than a manual role-parameter flip: skills
/// auto-load as the agent works.
#[test]
fn system_prompt_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        provider_response_id: Some("resp_skills".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // Simulate a skill becoming visible in the system prompt between
    // turns. `build_system_prompt` renders any `add_to_prompt: true`
    // skill into the prompt body, so inserting one here is the
    // narrowest way to make the system_prompt string drift without
    // touching unrelated state.
    h.discovered_skills.insert(
        tau_proto::SkillName::new("late-loaded"),
        crate::discovery::DiscoveredSkill {
            source_id: tau_proto::ConnectionId::from("test-ext"),
            description: "appears between turns".to_owned(),
            source: crate::discovery::DiscoveredSkillSource::File(std::path::PathBuf::from(
                "/tmp/late-loaded.md",
            )),
            add_to_prompt: true,
        },
    );

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    assert!(
        prompt2.previous_response_candidate.is_none(),
        "system-prompt drift (skill became visible) must clear the chain anchor"
    );
}

/// A tool registering mid-conversation must bust the chain — the
/// upstream stored its reasoning state against the *previous* tools
/// list, and chaining a request whose `tools` field grew (or shrank)
/// would silently mix new affordances into reasoning that never saw
/// them. Realistic trigger: an extension hot-registers a tool while
/// the user is mid-task.
#[test]
fn tools_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        provider_response_id: Some("resp_tools".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // A new tool appears between turns — same shape as an extension
    // hot-registering. `gather_tool_definitions` reads from the
    // registry on every send, so the next prompt's `tools` field
    // grows by one.
    h.registry.register(
        "test-ext",
        ToolSpec {
            name: ToolName::new("late_tool"),
            model_visible_name: None,
            description: Some("appeared between turns".to_owned()),
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    assert!(
        prompt2.previous_response_candidate.is_none(),
        "tools drift (new tool registered) must clear the chain anchor"
    );
}

/// Counterpart: when the per-request fingerprint inputs *don't*
/// change between turns, the chain anchor must remain valid. Locks
/// in the "compute fingerprint over (system_prompt, tools, params)"
/// surface — if a future change quietly mixes in some other input
/// that drifts across turns (e.g. cwd, current date, session id),
/// this test starts failing.
#[test]
fn stable_params_preserve_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_xyz".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    let prev = prompt2
        .previous_response_candidate
        .as_ref()
        .expect("chain should survive when no inputs drifted");
    assert_eq!(prev.provider_response_id, "resp_xyz");
}

/// A turn that didn't yield a `response_id` (Chat Completions
/// backend, an error, etc.) must NOT anchor a chain. The next prompt
/// has to be a full replay — pretending we have a chain we don't
/// would make the upstream API reject the next call.
#[test]
fn missing_response_id_leaves_chain_unset() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    assert!(
        prompt2.previous_response_candidate.is_none(),
        "no response_id on the prior turn means no chain"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_prompt_extends_completed_first_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let first = h
        .submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    assert_eq!(first, PromptSubmission::Dispatched);
    let spid1: SessionPromptId = "sp-0".into();
    let prompt1 = read_prompt_created(&h, &spid1);

    let second = h
        .submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    assert_eq!(second, PromptSubmission::Queued);

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid1,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
    .expect("finish first");

    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);
    assert!(
        prompt1.context_items.len() < prompt2.context_items.len(),
        "queued follow-up should extend the first prompt"
    );
    assert_eq!(
        &prompt2.context_items[..prompt1.context_items.len()],
        prompt1.context_items.as_slice()
    );
    let last = prompt2.context_items.last().expect("last item");
    assert!(matches!(
        last,
        ContextItem::Message(MessageItem {
            role: ContextRole::User,
            ..
        })
    ));
    assert_eq!(text_part(last), Some("second"));

    h.shutdown().expect("shutdown");
}

#[test]
fn restore_notice_elapsed_format_uses_minutes_hours_and_days() {
    // The restore notice is model-visible hidden context, so keep the elapsed
    // wording compact and deterministic while still warning about outside
    // changes since the durable transcript stopped.
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(59)))
            .contains("Less than 1 minute has passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(60)))
            .contains("1 minute has passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(42 * 60)))
            .contains("42 minutes have passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(2 * 60 * 60)))
            .contains("2 hours have passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(3 * 24 * 60 * 60)))
            .contains("3 days have passed since the last recorded session event")
    );
}

/// Regression: a cold-resumed session needs one hidden restore notice in the
/// first provider prompt, but startup itself must not send that notice as a
/// standalone turn or as prewarm-only context.
#[test]
fn resumed_startup_folds_restore_notice_before_first_user_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let two_hours_ago = tau_proto::UnixMicros::new(
        tau_proto::UnixMicros::now()
            .get()
            .saturating_sub(2 * 60 * 60 * 1_000_000),
    );
    seed_prior_user_message_at(&sp, "before restore", two_hours_ago);

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");

    assert!(h.prompt_conversations.is_empty());
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionPromptCreated(_)
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionPromptPrewarmRequested(prewarm)
            if prewarm
                .context_items
                .iter()
                .any(|item| text_part(item).is_some_and(is_restore_notice_prompt_text))
    )));
    assert_eq!(restore_notice_event_count(&h), 0);

    h.submit_user_prompt("s1".into(), "after restore".to_owned())
        .expect("submit first resumed prompt");
    let spid: SessionPromptId = "sp-0".into();
    let prompt = read_prompt_created(&h, &spid);
    let notice_pos = prompt
        .context_items
        .iter()
        .position(|item| text_part(item).is_some_and(is_restore_notice_prompt_text))
        .expect("restore notice in first prompt");
    let user_pos = prompt
        .context_items
        .iter()
        .position(|item| text_part(item) == Some("after restore"))
        .expect("user prompt in first prompt");
    let notice = restore_notice_context_text(&prompt).expect("restore notice text");

    assert!(notice_pos < user_pos);
    assert!(notice.contains("Previous session was interrupted and restored."));
    assert!(notice.contains("2 hours have passed since the last recorded session event"));
    assert!(notice.contains("state of the world might have changed"));
    assert_eq!(restore_notice_context_count(&prompt), 1);
    assert_eq!(restore_notice_event_count(&h), 1);

    h.shutdown().expect("shutdown");
}

/// The restore notice is a one-shot durable fact. Follow-up prompts and later
/// cold resumes may replay the original notice in history, but must not append
/// another copy.
#[test]
fn restore_notice_is_not_duplicated_by_followups_or_later_resumes() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_prior_user_message(&sp, "before restore");

    let notice = {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("first resume");

        h.submit_user_prompt("s1".into(), "first after restore".to_owned())
            .expect("submit first resumed prompt");
        let first_spid: SessionPromptId = "sp-0".into();
        let first_prompt = read_prompt_created(&h, &first_spid);
        let notice = restore_notice_context_text(&first_prompt)
            .expect("restore notice")
            .to_owned();
        assert_eq!(restore_notice_context_count(&first_prompt), 1);

        h.handle_provider_response_finished(provider_text_response(&first_spid, "first answer"))
            .expect("finish first prompt");
        h.submit_user_prompt("s1".into(), "second after restore".to_owned())
            .expect("submit second prompt");
        let second_spid: SessionPromptId = "sp-1".into();
        let second_prompt = read_prompt_created(&h, &second_spid);
        assert_eq!(context_text_count(&second_prompt, notice.as_str()), 1);
        assert_eq!(restore_notice_context_count(&second_prompt), 1);
        assert_eq!(restore_notice_event_count(&h), 1);

        h.shutdown().expect("shutdown");
        notice
    };
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("second resume");

        h.submit_user_prompt("s1".into(), "third after restore".to_owned())
            .expect("submit after second resume");
        let spid: SessionPromptId = "sp-0".into();
        let prompt = read_prompt_created(&h, &spid);
        assert_eq!(context_text_count(&prompt, notice.as_str()), 1);
        assert_eq!(restore_notice_context_count(&prompt), 1);
        assert_eq!(restore_notice_event_count(&h), 1);

        h.shutdown().expect("shutdown");
    }
}

/// Regression: a background placeholder without a later background result/error
/// means the real tool was lost across cold restore. Resume must publish a
/// durable background error, fold an internal interruption note before the next
/// user prompt, and let `wait` consume the restored error instead of hanging.
#[test]
fn resumed_lost_background_tool_gets_error_and_wait_returns() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "lost-bg", "slow_bg");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let notice = restored_background_notice("lost-bg");

    assert_eq!(background_error_count(&h, "lost-bg"), 1);
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "lost-bg" && error.message == notice
        )
    ));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionPromptCreated(_)
    )));

    h.submit_user_prompt("s1".into(), "after restore".to_owned())
        .expect("submit first resumed prompt");
    let first_spid: SessionPromptId = "sp-0".into();
    let first_prompt = read_prompt_created(&h, &first_spid);
    let notice_pos = first_prompt
        .context_items
        .iter()
        .position(|item| text_part(item) == Some(notice.as_str()))
        .expect("background interruption notice in first prompt");
    let user_pos = first_prompt
        .context_items
        .iter()
        .position(|item| text_part(item) == Some("after restore"))
        .expect("user prompt in first prompt");
    assert!(notice_pos < user_pos);

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: first_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-lost-bg".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("lost-bg".to_owned()),
            )]),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("wait for restored background call");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-lost-bg" && error.message == notice
    )));

    h.shutdown().expect("shutdown");
}

/// Resume should treat existing background results/errors as terminal. They are
/// replayed into the wait tracker, but no restored interruption error is
/// appended over the real outcome.
#[test]
fn resume_keeps_existing_background_completions() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "finished-bg", "slow_bg");
    seed_background_placeholder(&sp, "failed-bg", "slow_bg");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event(
            "s1",
            None,
            Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                call_id: "finished-bg".into(),
                tool_name: ToolName::new("slow_bg"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("finished".to_owned()),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("seed background result");
    store
        .append_session_event(
            "s1",
            None,
            Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
                call_id: "failed-bg".into(),
                tool_name: ToolName::new("slow_bg"),
                tool_type: tau_proto::ToolType::Function,
                message: "real failure".to_owned(),
                details: None,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("seed background error");
    drop(store);

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");

    assert_eq!(background_result_count(&h, "finished-bg"), 1);
    assert_eq!(background_error_count(&h, "finished-bg"), 0);
    assert_eq!(background_error_count(&h, "failed-bg"), 1);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.message == restored_background_notice(error.call_id.as_str())
    )));

    h.shutdown().expect("shutdown");
}

/// Completed background results restored from the session log should be
/// available to `wait({})`, not only to exact-id waits.
#[test]
fn resumed_completed_background_result_can_be_consumed_by_no_arg_wait() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "restored-any", "slow_bg");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event(
            "s1",
            None,
            Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                call_id: "restored-any".into(),
                tool_name: ToolName::new("slow_bg"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("restored output".to_owned()),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("seed background result");
    drop(store);

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    h.submit_user_prompt("s1".into(), "collect restored background".to_owned())
        .expect("submit first resumed prompt");
    let spid: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-restored-any".into(),
            name: ToolName::new("wait"),
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
    .expect("wait on restored completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-any"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-any")
                && cbor_map_text(&result.result, "output") == Some("restored output")
    )));

    h.shutdown().expect("shutdown");
}

/// Restored no-arg waits must replay completions by durable completion order,
/// not by the earlier provider-placeholder order.
#[test]
fn resumed_no_arg_wait_uses_restored_completion_event_order() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "restored-a", "slow_bg");
    seed_background_placeholder(&sp, "restored-b", "slow_bg");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    for (call_id, text) in [
        ("restored-b", "first restored output"),
        ("restored-a", "second restored output"),
    ] {
        store
            .append_session_event(
                "s1",
                None,
                Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                    call_id: call_id.into(),
                    tool_name: ToolName::new("slow_bg"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text(text.to_owned()),
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            )
            .expect("seed background result");
    }
    drop(store);

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = h.default_conversation_id.clone();
    h.handle_wait_tool_call(
        &cid,
        &wait_no_args_call("wait-restored-first"),
        ToolName::new("wait"),
    )
    .expect("consume first restored completion");
    h.handle_wait_tool_call(
        &cid,
        &wait_no_args_call("wait-restored-second"),
        ToolName::new("wait"),
    )
    .expect("consume second restored completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-first"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-b")
                && cbor_map_text(&result.result, "output") == Some("first restored output")
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-second"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-a")
                && cbor_map_text(&result.result, "output") == Some("second restored output")
    )));

    h.shutdown().expect("shutdown");
}

/// The restored background error is durable. A later cold resume must observe
/// the existing error and avoid appending a duplicate.
#[test]
fn repeated_resume_does_not_duplicate_background_errors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "lost-once", "slow_bg");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("first resume");
        assert_eq!(background_error_count(&h, "lost-once"), 1);
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("second resume");
        assert_eq!(background_error_count(&h, "lost-once"), 1);
        h.shutdown().expect("shutdown");
    }
}

#[test]
fn switch_session_rebinds_default_conversation() {
    // Regression: `/new` flips `current_session_id` but used to leave
    // the default conversation pointing at the old session, which made
    // the next user prompt panic in `dispatch_user_prompt`'s
    // assert_eq!.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"
    h.selected_model = Some("test/model".into());
    let model: tau_proto::ModelId = "test/model".into();
    h.current_session_state.context_input_tokens = Some(92_000);
    h.current_session_state.context_cached_tokens = Some(90_000);
    h.current_session_state.context_percent_used = Some(92);
    h.current_session_state.token_usage.start_request(&model);
    h.current_session_state
        .token_usage
        .add_sent(&model, 819_300, 750_000);
    h.current_session_state
        .token_usage
        .add_received(&model, 34_000);

    let cid = h.default_conversation_id.clone();
    assert_eq!(h.conversations[&cid].session_id.as_str(), "s1");

    let shell_conn = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch");

    let mut saw_session_dir = false;
    let mut cursor = 0;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        if let Event::HarnessSessionDir(session_dir) = &entry.event
            && session_dir.session_id == "s2"
            && session_dir.path.ends_with("s2")
            && session_dir.status == tau_proto::SessionDirStatus::New
        {
            saw_session_dir = true;
        }
    }
    assert!(saw_session_dir, "switch must announce the new session dir");

    assert_eq!(h.current_session_id.as_str(), "s2");
    assert_eq!(h.current_session_state.context_input_tokens, None);
    assert_eq!(h.current_session_state.context_cached_tokens, None);
    assert_eq!(h.current_session_state.context_percent_used, None);
    assert_eq!(
        h.current_session_state.token_usage,
        tau_proto::TokenUsageStats::default()
    );
    assert_eq!(
        h.conversations[&cid].session_id.as_str(),
        "s2",
        "default conversation must follow the bound session id",
    );

    // Drive the new session through init so submit_user_prompt
    // actually dispatches (rather than queuing).
    h.handle_extension_event(
        &shell_conn,
        Frame::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s2".into(),
            },
        )),
    )
    .expect("ready");

    let submission = h
        .submit_user_prompt("s2".into(), "hello".to_owned())
        .expect("submit");
    assert_eq!(submission, PromptSubmission::Dispatched);

    h.shutdown().expect("shutdown");
}

#[test]
fn user_prompt_auto_compacts_before_submission() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    enable_remote_compaction_for_test_model(&mut h);

    append_user_message_via_event(&mut h, "s1", "earlier question");
    let cid = h.default_conversation_id.clone();
    h.publish_for_conversation(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            session_prompt_id: "sp-old".into(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,

                content: vec![ContentPart::Text {
                    text: "earlier answer".to_owned(),
                }],

                phase: None,
            })],

            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            usage: match (None, None, None) {
                (None, None, None) => None,
                (input_tokens, cached_tokens, output_tokens) => {
                    Some(tau_proto::ProviderTokenUsage {
                        model: None,
                        prompt_sent_tokens: input_tokens.unwrap_or(0),
                        prompt_cached_tokens: cached_tokens.unwrap_or(0),
                        response_received_tokens: output_tokens.unwrap_or(0),
                        stats: Default::default(),
                    })
                }
            },
            originator: tau_proto::PromptOriginator::User,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    h.current_session_state.context_input_tokens = Some(950);
    h.current_session_state.context_percent_used = Some(95);
    let baseline_seq = h.event_log.next_seq();

    h.dispatch_user_prompt("s1".into(), "new question".to_owned())
        .expect("dispatch");

    assert_eq!(
        h.pending_compactions.len(),
        1,
        "compaction should start first"
    );
    assert!(matches!(
        h.conversations[&cid].turn_state,
        ConversationTurnState::Compacting
    ));
    assert!(
        h.store
            .session("s1")
            .expect("session")
            .current_branch()
            .iter()
            .all(|entry| {
                !matches!(
                    entry,
                    SessionEntry::UserInput { items }
                        if items.iter().any(|item| text_part(item) == Some("new question"))
                )
            }),
        "user prompt must not be persisted until compaction finishes"
    );

    let (summary_cid, summary_spid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid != &cid).then_some((prompt_cid.clone(), spid.clone()))
        })
        .expect("compaction prompt");
    let summary_prompt = read_compaction_requested(&h, &summary_spid);
    assert!(
        summary_prompt.previous_response_candidate.is_none(),
        "compaction requests should not reuse previous_response_id chaining"
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: summary_spid,
        output_items: vec![ContextItem::Compaction(tau_proto::OpaqueProviderItem(
            CborValue::Map(vec![(
                CborValue::Text("type".to_owned()),
                CborValue::Text("summary".to_owned()),
            )]),
        ))],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: match (Some(400), None, Some(40)) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: HARNESS_CONNECTION_ID.into(),
            query_id: format!("auto-compact-{cid}"),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("summary finished");

    assert!(!h.conversations.contains_key(&summary_cid));
    assert!(h.pending_compactions.is_empty());
    assert!(matches!(
        h.conversations[&cid].turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));
    assert_eq!(h.current_session_state.context_input_tokens, None);
    assert_eq!(h.current_session_state.context_percent_used, None);

    let mut cursor = baseline_seq;
    let mut started_original_tokens = None;
    let mut compacted_tokens = None;
    let mut finished_tokens = None;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        match entry.event {
            Event::SessionCompactionStarted(started) => {
                started_original_tokens = Some(started.original_input_tokens);
            }
            Event::SessionCompacted(compacted) => {
                compacted_tokens = Some((
                    compacted.original_input_tokens,
                    compacted.compacted_input_tokens,
                ));
            }
            Event::SessionCompactionFinished(finished) => {
                finished_tokens = Some((
                    finished.original_input_tokens,
                    finished.compacted_input_tokens,
                ));
            }
            _ => {}
        }
    }
    assert_eq!(started_original_tokens, Some(Some(950)));
    assert_eq!(compacted_tokens, Some((Some(950), Some(40))));
    assert_eq!(finished_tokens, Some((Some(950), Some(40))));

    let branch = h.store.session("s1").expect("session").current_branch();
    assert!(matches!(
        branch.get(branch.len().saturating_sub(2)),
        Some(SessionEntry::Compaction { replacement_window }) if !replacement_window.is_empty()
    ));
    assert!(matches!(
        branch.last(),
        Some(SessionEntry::UserInput { items })
            if items.iter().any(|item| text_part(item) == Some("new question"))
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn compaction_without_provider_usage_estimates_compacted_tokens_from_replacement_window() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    // Use a provider that advertises compaction but does not auto-answer the
    // request, so the test owns the no-usage response below.
    let mut h = quiet_provider_harness(&sp).expect("start");

    enable_remote_compaction_for_test_model(&mut h);

    let cid = h.default_conversation_id.clone();
    // Manual compaction reads from the conversation head. Seed via the
    // conversation-aware publisher rather than only folding a durable event.
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "earlier question".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.current_session_state.context_input_tokens = Some(950);
    h.current_session_state.context_percent_used = Some(95);
    let baseline_seq = h.event_log.next_seq();

    h.handle_compact_request("s1".into());

    let (_summary_cid, summary_spid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid != &cid).then_some((prompt_cid.clone(), spid.clone()))
        })
        .expect("compaction prompt");

    // The real OpenAI standalone compaction path returns provider-owned
    // replacement items but normally no usage block. The harness still needs a
    // post-compaction context-size chip, so it estimates from the exact items it
    // will replay as the new prompt prefix.
    let summary_text = "compacted summary sentence. ".repeat(160);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: summary_spid,
        output_items: vec![openai_compaction_summary_item(&summary_text)],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: HARNESS_CONNECTION_ID.into(),
            query_id: format!("auto-compact-{cid}"),
        },
        backend: Some(responses_backend()),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("summary finished");

    let mut compacted_tokens = None;
    let mut finished_tokens = None;
    let mut cursor = baseline_seq;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        match entry.event {
            Event::SessionCompacted(compacted) => {
                compacted_tokens = compacted.compacted_input_tokens;
            }
            Event::SessionCompactionFinished(finished) => {
                finished_tokens = finished.compacted_input_tokens;
            }
            _ => {}
        }
    }

    let compacted_tokens = compacted_tokens.expect("durable compacted token estimate");
    assert!(
        compacted_tokens >= 1_000,
        "estimate should reflect the summary payload size, got {compacted_tokens}"
    );
    assert_eq!(finished_tokens, Some(compacted_tokens));

    h.shutdown().expect("shutdown");
}

#[test]
fn failed_compaction_does_not_report_compacted_tokens_from_provider_usage() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    enable_remote_compaction_for_test_model(&mut h);

    let cid = h.default_conversation_id.clone();
    // Manual compaction reads from the conversation head. Seed via the
    // conversation-aware publisher rather than only folding a durable event.
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "earlier question".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.current_session_state.context_input_tokens = Some(950);
    h.current_session_state.context_percent_used = Some(95);
    let baseline_seq = h.event_log.next_seq();

    h.handle_compact_request("s1".into());

    let (_summary_cid, summary_spid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid != &cid).then_some((prompt_cid.clone(), spid.clone()))
        })
        .expect("compaction prompt");

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: summary_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "LLM error: no summary available".to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: Some(tau_proto::ProviderTokenUsage {
            model: None,
            prompt_sent_tokens: 400,
            prompt_cached_tokens: 0,
            response_received_tokens: 123,
            stats: Default::default(),
        }),
        originator: tau_proto::PromptOriginator::Extension {
            name: HARNESS_CONNECTION_ID.into(),
            query_id: format!("auto-compact-{cid}"),
        },
        backend: Some(responses_backend()),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("summary finished");

    let mut saw_compacted = false;
    let mut finished_tokens = None;
    let mut finished_outcome = None;
    let mut cursor = baseline_seq;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        match entry.event {
            Event::SessionCompacted(_) => saw_compacted = true,
            Event::SessionCompactionFinished(finished) => {
                finished_tokens = finished.compacted_input_tokens;
                finished_outcome = Some(finished.outcome);
            }
            _ => {}
        }
    }

    assert!(
        !saw_compacted,
        "failed compaction must not emit SessionCompacted"
    );
    assert_eq!(finished_tokens, None);
    assert_eq!(
        finished_outcome,
        Some(tau_proto::SessionCompactionOutcome::Failed)
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn user_prompt_does_not_auto_compact_without_context_percent_signal() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    enable_remote_compaction_for_test_model(&mut h);

    let large_text = "earlier context ".repeat(40);
    append_user_message_via_event(&mut h, "s1", &large_text);
    let cid = h.default_conversation_id.clone();
    h.publish_for_conversation(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            session_prompt_id: "sp-old".into(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,

                content: vec![ContentPart::Text {
                    text: large_text.clone(),
                }],

                phase: None,
            })],

            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            usage: match (None, None, None) {
                (None, None, None) => None,
                (input_tokens, cached_tokens, output_tokens) => {
                    Some(tau_proto::ProviderTokenUsage {
                        model: None,
                        prompt_sent_tokens: input_tokens.unwrap_or(0),
                        prompt_cached_tokens: cached_tokens.unwrap_or(0),
                        response_received_tokens: output_tokens.unwrap_or(0),
                        stats: Default::default(),
                    })
                }
            },
            originator: tau_proto::PromptOriginator::User,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    h.current_session_state.context_percent_used = None;

    h.dispatch_user_prompt("s1".into(), "new question".to_owned())
        .expect("dispatch");

    assert!(h.pending_compactions.is_empty());
    assert!(matches!(
        h.conversations[&cid].turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));
    let spid = h.conversations[&cid]
        .in_flight_prompt
        .clone()
        .expect("in-flight prompt");
    let prompt = read_prompt_created(&h, &spid);
    assert_eq!(
        prompt.context_items.last().and_then(text_part),
        Some("new question")
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn manual_compact_forces_compaction_without_followup_turn() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    enable_remote_compaction_for_test_model(&mut h);

    append_user_message_via_event(&mut h, "s1", "earlier question");
    let cid = h.default_conversation_id.clone();
    h.publish_for_conversation(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            session_prompt_id: "sp-old".into(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,

                content: vec![ContentPart::Text {
                    text: "earlier answer".to_owned(),
                }],

                phase: None,
            })],

            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            usage: match (None, None, None) {
                (None, None, None) => None,
                (input_tokens, cached_tokens, output_tokens) => {
                    Some(tau_proto::ProviderTokenUsage {
                        model: None,
                        prompt_sent_tokens: input_tokens.unwrap_or(0),
                        prompt_cached_tokens: cached_tokens.unwrap_or(0),
                        response_received_tokens: output_tokens.unwrap_or(0),
                        stats: Default::default(),
                    })
                }
            },
            originator: tau_proto::PromptOriginator::User,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );

    h.handle_compact_request("s1".into());

    assert_eq!(h.pending_compactions.len(), 1);
    let (summary_cid, summary_spid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid != &cid).then_some((prompt_cid.clone(), spid.clone()))
        })
        .expect("compaction prompt");
    let summary_prompt = read_compaction_requested(&h, &summary_spid);
    assert!(summary_prompt.previous_response_candidate.is_none());

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: summary_spid,
        output_items: vec![ContextItem::Compaction(tau_proto::OpaqueProviderItem(
            CborValue::Map(vec![(
                CborValue::Text("type".to_owned()),
                CborValue::Text("summary".to_owned()),
            )]),
        ))],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: match (Some(300), None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: HARNESS_CONNECTION_ID.into(),
            query_id: format!("auto-compact-{cid}"),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("summary finished");

    assert!(!h.conversations.contains_key(&summary_cid));
    assert!(h.pending_compactions.is_empty());
    assert!(matches!(
        h.conversations[&cid].turn_state,
        ConversationTurnState::Idle
    ));
    assert!(h.conversations[&cid].in_flight_prompt.is_none());

    let branch = h.store.session("s1").expect("session").current_branch();
    assert!(matches!(
        branch.last(),
        Some(SessionEntry::Compaction { replacement_window }) if !replacement_window.is_empty()
    ));

    h.shutdown().expect("shutdown");
}

/// Regression for the deferred-dispatch gate: `SessionCompactionStarted`
/// is not a user-message event, so a compaction summary prompt parked
/// behind interception must wait for publish-idle, not for the next
/// user-message fold that may never come.
#[test]
fn intercepted_compaction_started_dispatches_summary_after_publish_idle() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);

    let _interceptor_events = connect_test_tool(&mut h, "conn-interceptor");
    h.interceptors.replace_for_connection(
        "conn-interceptor",
        tau_proto::ExtensionName::from("test-interceptor"),
        vec![EventSelector::Exact(
            tau_proto::EventName::SESSION_COMPACTION_STARTED,
        )],
        InterceptionPriority::new(0),
    );

    let cid = h.default_conversation_id.clone();
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "earlier question".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.current_session_state.context_input_tokens = Some(950);
    h.current_session_state.context_percent_used = Some(95);

    h.dispatch_user_prompt("s1".into(), "new question".to_owned())
        .expect("dispatch");

    assert_eq!(h.pending_compactions.len(), 1);
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);
    assert!(
        h.prompt_conversations.is_empty(),
        "summary prompt must not dispatch until the intercepted start event commits",
    );

    h.handle_intercept_reply(
        "conn-interceptor",
        InterceptReply {
            action: InterceptAction::Pass(None),
        },
    );

    assert!(h.pending_user_prompt_dispatches.is_empty());
    assert!(h.pending_publish_idle_dispatches.is_empty());
    let (summary_cid, summary_spid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid != &cid).then_some((prompt_cid.clone(), spid.clone()))
        })
        .expect("compaction prompt");
    assert!(h.pending_compactions.contains_key(&summary_cid));
    let summary_prompt = read_compaction_requested(&h, &summary_spid);
    assert!(matches!(
        summary_prompt.originator,
        tau_proto::PromptOriginator::Extension { ref query_id, .. } if query_id == "auto-compact-default"
    ));

    h.shutdown().expect("shutdown");
}

fn enable_remote_compaction_for_test_model(h: &mut Harness) {
    h.selected_model = Some("test/model".into());
    h.provider_model_info.insert(
        "test/model".into(),
        tau_proto::ProviderModelInfo {
            id: "test/model".into(),
            display_name: None,
            default_affinity: 0,
            context_window: 1_000,
            efforts: vec![tau_proto::Effort::Medium],
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: true,
        },
    );
}

fn instant_background_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: None,
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(tau_proto::BackgroundSupport::Instant),
    }
}

fn start_manual_compaction_for_test(
    h: &mut Harness,
    cid: &ConversationId,
) -> (ConversationId, SessionPromptId) {
    h.handle_compact_request("s1".into());
    h.prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            h.pending_compactions
                .contains_key(prompt_cid)
                .then_some((prompt_cid.clone(), spid.clone()))
        })
        .filter(|(summary_cid, _)| summary_cid != cid)
        .expect("compaction prompt")
}

fn finish_compaction_for_test(
    h: &mut Harness,
    summary_spid: SessionPromptId,
    cid: &ConversationId,
) {
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: summary_spid,
        output_items: vec![openai_compaction_summary_item("compacted branch")],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: HARNESS_CONNECTION_ID.into(),
            query_id: format!("auto-compact-{cid}"),
        },
        backend: Some(responses_backend()),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("summary finished");
}

fn active_prompt_for(h: &Harness, cid: &ConversationId) -> SessionPromptId {
    h.conversations
        .get(cid)
        .expect("conversation exists")
        .in_flight_prompt
        .clone()
        .expect("active prompt")
}

fn start_background_tool_and_finish_placeholder_turn(
    h: &mut Harness,
    cid: &ConversationId,
    call_id: &str,
    tool_name: &str,
) {
    h.publish_for_conversation(
        cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: format!("run {tool_name}"),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let spid: SessionPromptId = format!("sp-{call_id}").into();
    seed_agent_thinking(h, cid, spid.as_str());
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: ToolName::new(tool_name),
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
    .expect("start background tool");
    assert!(h.tool_turn.is_backgrounded(&ToolCallId::from(call_id)));

    let placeholder_followup = active_prompt_for(h, cid);
    h.handle_provider_response_finished(provider_text_response(
        &placeholder_followup,
        "placeholder acknowledged",
    ))
    .expect("finish placeholder followup");
    assert!(matches!(
        h.conversations
            .get(cid)
            .expect("conversation exists")
            .turn_state,
        ConversationTurnState::Idle
    ));
}

#[test]
fn wait_returns_internal_background_error_after_extension_disconnect() {
    // A backgrounded call belongs to its call id, not to a future provider
    // registration. When the extension disconnects, `wait` must consume the
    // synthesized background error immediately instead of hanging.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-bg-disconnect");
    h.registry.register(
        "conn-bg-disconnect",
        instant_background_test_tool_spec("slow_disconnect"),
    );

    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-disconnect".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        call_id.as_str(),
        "slow_disconnect",
    );
    assert_eq!(
        h.pending_tool_providers
            .get(&call_id)
            .map(|provider| provider.as_str()),
        Some("conn-bg-disconnect")
    );

    h.handle_disconnect("conn-bg-disconnect");

    let expected = extension_disconnected_background_tool_call_error_message(&call_id);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == call_id.as_str()
                && error.message == expected
    )));

    let _replacement_events = connect_test_tool(&mut h, "conn-bg-replacement");
    h.registry.register(
        "conn-bg-replacement",
        instant_background_test_tool_spec("slow_disconnect"),
    );

    let wait_call = AgentToolCall {
        id: "wait-bg-disconnect".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(call_id.to_string()),
        )]),
        display: None,
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("wait returns disconnected background error");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-bg-disconnect"
                && error.message == expected
    )));

    h.shutdown().expect("shutdown");
}

/// A no-arg wait that is already blocked when its background call completes
/// must consume the result and suppress the normal internal completion prompt.
#[test]
fn no_arg_wait_before_background_completion_suppresses_completion_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-bg-any-before");
    h.registry.register(
        "conn-bg-any-before",
        instant_background_test_tool_spec("slow_any_before"),
    );

    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-any-before".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        call_id.as_str(),
        "slow_any_before",
    );

    let wait_call = wait_no_args_call("wait-any-before");
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start no-arg wait");
    h.handle_extension_event_inner(
        "conn-bg-any-before",
        Event::ToolResult(final_tool_result(
            call_id.as_str(),
            "slow_any_before",
            "background done",
        )),
    )
    .expect("background result");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-any-before"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some(call_id.as_str())
                && cbor_map_text(&result.result, "output") == Some("background done")
    )));
    let completion_prompt = background_completion_prompt(&call_id);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionPromptSteered(steered) if steered.text == completion_prompt
    )));
    assert!(
        h.conversations[&cid]
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt)
    );

    h.shutdown().expect("shutdown");
}

/// If a completion notice is queued but not steered yet, `wait({})` should
/// remove it while returning the already-completed background result.
#[test]
fn no_arg_wait_after_background_completion_removes_queued_completion_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-any-after".into();

    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.record_wait_background_result(tau_proto::ToolBackgroundResult {
        call_id: call_id.clone(),
        tool_name: ToolName::new("slow_any_after"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("already done".to_owned()),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    seed_tools_running(&mut h, &cid, Vec::new());
    h.queue_background_completion_prompt(&cid, &call_id);
    let completion_prompt = background_completion_prompt(&call_id);
    assert!(
        h.conversations[&cid]
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == completion_prompt && prompt.is_internal())
    );

    let wait_call = wait_no_args_call("wait-any-after");
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("consume queued completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-any-after"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some(call_id.as_str())
                && cbor_map_text(&result.result, "output") == Some("already done")
    )));
    assert!(
        h.suppressed_background_completion_prompts
            .contains(&call_id)
    );
    assert!(
        h.conversations[&cid]
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt)
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a user prompt submitted during manual compaction used to remain
/// queued forever because `Resume::None` restored the conversation to `Idle`
/// but never re-ran the normal pending-prompt drain.
#[test]
fn manual_compaction_drains_user_prompt_queued_while_compacting() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);

    let cid = h.default_conversation_id.clone();
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "earlier question".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let (_summary_cid, summary_spid) = start_manual_compaction_for_test(&mut h, &cid);

    let submission = h
        .submit_user_prompt("s1".into(), "queued after compact".to_owned())
        .expect("submit");
    assert_eq!(submission, PromptSubmission::Queued);
    finish_compaction_for_test(&mut h, summary_spid, &cid);

    let prompt_spid = active_prompt_for(&h, &cid);
    let prompt = read_prompt_created(&h, &prompt_spid);
    assert_eq!(
        prompt.context_items.last().and_then(text_part),
        Some("queued after compact")
    );
    assert!(h.conversations[&cid].pending_prompts.is_empty());

    h.shutdown().expect("shutdown");
}

/// Regression: a background tool can complete while manual compaction holds the
/// owning conversation in `Compacting`. Finishing compaction must fold that
/// internal completion notice and dispatch it instead of leaving it queued.
#[test]
fn background_completion_during_manual_compaction_is_dispatched_after_finish() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let _tool_events = connect_test_tool(&mut h, "conn-bg");
    h.registry
        .register("conn-bg", instant_background_test_tool_spec("slow_bg"));

    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-during-compact".into();
    start_background_tool_and_finish_placeholder_turn(&mut h, &cid, call_id.as_str(), "slow_bg");
    let (_summary_cid, summary_spid) = start_manual_compaction_for_test(&mut h, &cid);

    h.handle_extension_event_inner(
        "conn-bg",
        Event::ToolResult(final_tool_result(
            call_id.as_str(),
            "slow_bg",
            "background output",
        )),
    )
    .expect("background result during compaction");
    assert!(h.conversations[&cid].pending_prompts.iter().any(|prompt| {
        prompt.is_internal() && prompt.text == background_completion_prompt(&call_id)
    }));

    finish_compaction_for_test(&mut h, summary_spid, &cid);

    let prompt_spid = active_prompt_for(&h, &cid);
    let prompt = read_prompt_created(&h, &prompt_spid);
    assert!(
        prompt.context_items.iter().any(|item| {
            text_part(item) == Some(background_completion_prompt(&call_id).as_str())
        })
    );
    assert!(h.conversations[&cid].pending_prompts.is_empty());

    h.shutdown().expect("shutdown");
}

/// Regression: auto-compaction holds the triggering user prompt out of the log.
/// If a background completion arrives before the summary finishes, the resumed
/// prompt must include both the internal completion notice and the held user
/// text in one dispatch, with the internal notice folded first.
#[test]
fn background_completion_during_user_prompt_compaction_precedes_held_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let _tool_events = connect_test_tool(&mut h, "conn-bg-held");
    h.registry.register(
        "conn-bg-held",
        instant_background_test_tool_spec("slow_held"),
    );

    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-held".into();
    start_background_tool_and_finish_placeholder_turn(&mut h, &cid, call_id.as_str(), "slow_held");
    h.current_session_state.context_input_tokens = Some(950);
    h.current_session_state.context_percent_used = Some(95);

    h.dispatch_user_prompt("s1".into(), "new question".to_owned())
        .expect("dispatch starts compaction");
    let (_summary_cid, summary_spid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            h.pending_compactions
                .contains_key(prompt_cid)
                .then_some((prompt_cid.clone(), spid.clone()))
        })
        .expect("compaction prompt");

    h.handle_extension_event_inner(
        "conn-bg-held",
        Event::ToolResult(final_tool_result(
            call_id.as_str(),
            "slow_held",
            "held background output",
        )),
    )
    .expect("background result during held prompt compaction");
    finish_compaction_for_test(&mut h, summary_spid, &cid);

    let prompt_spid = active_prompt_for(&h, &cid);
    let prompt = read_prompt_created(&h, &prompt_spid);
    let completion_pos = prompt
        .context_items
        .iter()
        .position(|item| text_part(item) == Some(background_completion_prompt(&call_id).as_str()))
        .expect("completion notice in resumed prompt");
    let user_pos = prompt
        .context_items
        .iter()
        .position(|item| text_part(item) == Some("new question"))
        .expect("held user prompt in resumed prompt");
    assert!(completion_pos < user_pos);
    assert!(h.conversations[&cid].pending_prompts.is_empty());

    h.shutdown().expect("shutdown");
}

/// Regression: compaction is transcript-only. A real background result that
/// arrives while the transcript is compacting must remain available to the
/// process-local wait tracker, and the wait result must fold onto the compacted
/// branch rather than resurrecting pre-compaction history.
#[test]
fn wait_after_compaction_returns_background_result_completed_during_compaction() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let _tool_events = connect_test_tool(&mut h, "conn-bg-wait");
    h.registry.register(
        "conn-bg-wait",
        instant_background_test_tool_spec("slow_wait"),
    );

    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-wait".into();
    start_background_tool_and_finish_placeholder_turn(&mut h, &cid, call_id.as_str(), "slow_wait");
    let (_summary_cid, summary_spid) = start_manual_compaction_for_test(&mut h, &cid);
    h.handle_extension_event_inner(
        "conn-bg-wait",
        Event::ToolResult(final_tool_result(
            call_id.as_str(),
            "slow_wait",
            "waited output",
        )),
    )
    .expect("background result during compaction");
    finish_compaction_for_test(&mut h, summary_spid, &cid);

    let notification_spid = active_prompt_for(&h, &cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: notification_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-call-after-compact".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text(call_id.to_string()),
            )]),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("wait response after compaction");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-call-after-compact"
                && matches!(&result.result, CborValue::Text(text) if text == "waited output")
    )));
    let followup_spid = active_prompt_for(&h, &cid);
    let prompt = read_prompt_created(&h, &followup_spid);
    assert!(
        prompt
            .context_items
            .iter()
            .any(|item| matches!(item, ContextItem::Compaction(_)))
    );
    assert!(
        prompt
            .context_items
            .iter()
            .filter_map(tool_call_id)
            .any(|id| id == "wait-call-after-compact")
    );
    assert!(
        prompt
            .context_items
            .iter()
            .filter_map(tool_result_id)
            .any(|id| id == "wait-call-after-compact")
    );
    let visible_text: Vec<&str> = prompt.context_items.iter().filter_map(text_part).collect();
    assert!(visible_text.iter().all(|text| {
        !text.contains("run slow_wait") && !text.contains("placeholder acknowledged")
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn delegate_followup_auto_compacts_from_own_context_signal() {
    // Sub-agent sessions are normal conversations: if their own
    // context signal crosses the threshold, they must get a compaction
    // pass instead of being handicapped behind the main agent.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);

    let side_cid = ConversationId::new("start-agent-__harness__-delegate-1");
    let originator = tau_proto::PromptOriginator::Extension {
        name: HARNESS_CONNECTION_ID.into(),
        query_id: "delegate-1".to_owned(),
    };
    let mut side_conv = Conversation::new(
        side_cid.clone(),
        "s1".into(),
        originator.clone(),
        None,
        Some(HARNESS_CONNECTION_ID.into()),
    );
    side_conv.parent_tool_call_id = Some("call-delegate".into());
    side_conv.context_input_tokens = Some(950);
    side_conv.context_percent_used = Some(95);
    h.conversations.insert(side_cid.clone(), side_conv);

    let baseline_seq = h.event_log.next_seq();
    assert!(h.should_auto_compact_for_conversation(&side_cid));
    assert!(h.maybe_start_auto_compaction_for_followup(&side_cid));
    assert_eq!(h.pending_compactions.len(), 1);
    assert!(matches!(
        h.conversations[&side_cid].turn_state,
        ConversationTurnState::Compacting
    ));

    let (summary_cid, summary_spid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid != &side_cid).then_some((prompt_cid.clone(), spid.clone()))
        })
        .expect("compaction prompt");
    assert!(h.pending_compactions.contains_key(&summary_cid));
    let summary_prompt = read_compaction_requested(&h, &summary_spid);
    assert!(matches!(
        summary_prompt.originator,
        tau_proto::PromptOriginator::Extension { ref name, ref query_id }
            if name.as_str() == HARNESS_CONNECTION_ID && query_id == "auto-compact-start-agent-__harness__-delegate-1"
    ));

    let mut cursor = baseline_seq;
    let mut started = None;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        if let Event::SessionCompactionStarted(event) = entry.event {
            started = Some((event.originator, event.original_input_tokens));
            break;
        }
    }
    assert_eq!(started, Some((originator, Some(950))));

    h.shutdown().expect("shutdown");
}

#[test]
fn auto_compaction_uses_conversation_role_threshold() {
    // Roles can opt into earlier compaction than Tau's default threshold. The
    // decision must use the role bound to the active conversation, including
    // delegate side conversations, rather than only the global selected role.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.available_roles.insert(
        "early-compact".to_owned(),
        tau_config::settings::AgentRole {
            compaction_threshold: Some(70),
            ..Default::default()
        },
    );

    let side_cid = ConversationId::new("start-agent-__harness__-delegate-early");
    let mut side_conv = Conversation::new(
        side_cid.clone(),
        "s1".into(),
        tau_proto::PromptOriginator::Extension {
            name: HARNESS_CONNECTION_ID.into(),
            query_id: "delegate-early".to_owned(),
        },
        None,
        Some(HARNESS_CONNECTION_ID.into()),
    );
    side_conv.parent_tool_call_id = Some("call-delegate".into());
    side_conv.role = Some("early-compact".to_owned());
    side_conv.context_percent_used = Some(75);
    h.conversations.insert(side_cid.clone(), side_conv);

    assert!(h.should_auto_compact_for_conversation(&side_cid));

    h.shutdown().expect("shutdown");
}

#[test]
fn side_conversation_auto_compaction_ignores_default_context_signal() {
    // Regression for session fedimint-1hj5h9: a delegate side
    // conversation must not inherit the main/default context percent.
    // Otherwise the wrong conversation can enter compaction, and its
    // summary prompt can wedge the target in `Compacting`.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);

    let side_cid = ConversationId::new("start-agent-__harness__-delegate-1");
    let mut side_conv = Conversation::new(
        side_cid.clone(),
        "s1".into(),
        tau_proto::PromptOriginator::Extension {
            name: HARNESS_CONNECTION_ID.into(),
            query_id: "delegate-1".to_owned(),
        },
        None,
        Some(HARNESS_CONNECTION_ID.into()),
    );
    side_conv.parent_tool_call_id = Some("call-delegate".into());
    h.conversations.insert(side_cid.clone(), side_conv);
    h.current_session_state.context_percent_used = Some(95);

    assert!(!h.should_auto_compact_for_conversation(&side_cid));
    assert!(!h.maybe_start_auto_compaction_for_followup(&side_cid));
    assert!(h.pending_compactions.is_empty());
    assert!(matches!(
        h.conversations[&side_cid].turn_state,
        ConversationTurnState::Idle
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn incoming_user_prompt_does_not_preempt_compaction_summary() {
    // A compaction summary conversation is an internal lifecycle for
    // the default turn, not a disposable extension side query. If an
    // incoming prompt cancels it, the target conversation stays stuck
    // in `Compacting` with no prompt left to finish the lifecycle.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    enable_remote_compaction_for_test_model(&mut h);

    append_user_message_via_event(&mut h, "s1", "earlier question");
    h.current_session_state.context_input_tokens = Some(950);
    h.current_session_state.context_percent_used = Some(95);
    let cid = h.default_conversation_id.clone();
    h.dispatch_user_prompt("s1".into(), "new question".to_owned())
        .expect("dispatch");

    let (summary_spid, summary_cid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid != &cid).then_some((spid.clone(), prompt_cid.clone()))
        })
        .expect("compaction prompt");

    let submission = h
        .submit_user_prompt("s1".into(), "queued behind compaction".to_owned())
        .expect("submit");
    assert_eq!(submission, PromptSubmission::Queued);
    assert_eq!(h.pending_compactions.len(), 1);
    assert_eq!(
        h.prompt_conversations.get(&summary_spid),
        Some(&summary_cid)
    );
    assert!(!h.canceled_prompts.contains(&summary_spid));
    assert!(matches!(
        h.conversations[&summary_cid].turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));
    assert!(
        h.conversations[&cid]
            .pending_prompts
            .iter()
            .any(|prompt| prompt == "queued behind compaction")
    );

    h.shutdown().expect("shutdown");
}

/// While a parent's `delegate` tool call is in flight, the harness
/// must still dispatch the spawned side conversation's prompt
/// immediately — the parent's `ToolsRunning` turn state is logically
/// independent from the side conv's own turn. The two failure modes
/// this test pins down: (1) the side prompt gets queued behind the
/// parent's pending tool result and never goes out (deadlock), and
/// (2) the parent's `ToolsRunning` state gets clobbered when the
/// side conv finishes, leaving the parent unable to receive its
/// `ToolResult`. Uses the real delegate shape (`tool_call_id: Some`).
#[test]
fn start_agent_request_dispatches_while_tool_is_running_and_restores_turn() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("side_source"),
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
    let spid: SessionPromptId = "sp-main".into();
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("side_source"),
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
    .expect("tool response");

    assert!(matches!(h.turn_state, TurnState::Idle));
    let default_turn = &h
        .conversations
        .get(&h.default_conversation_id)
        .expect("default conversation")
        .turn_state;
    assert!(matches!(
        default_turn,
        ConversationTurnState::ToolsRunning { .. }
    ));
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q1".to_owned(),
            agent_id: "test-agent-q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    assert!(
        h.conversations
            .values()
            .all(|conv| conv.pending_prompts.is_empty()),
        "side prompt must dispatch immediately"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::UiPromptSubmitted(prompt)
            if prompt.text == "side task"
                && prompt.target_agent_id.as_deref() == Some("test-agent-q1")
    )));
    assert!(matches!(h.turn_state, TurnState::Idle));

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "delegated answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q1".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side finished");

    assert!(matches!(h.turn_state, TurnState::Idle));
    let default_turn = &h
        .conversations
        .get(&h.default_conversation_id)
        .expect("default conversation")
        .turn_state;
    assert!(matches!(
        default_turn,
        ConversationTurnState::ToolsRunning { .. }
    ));
    let events = delegate_events.lock().expect("delegate events");
    let result = events
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Event(Event::StartAgentResult(result)) if result.query_id == "q1" => {
                Some(result)
            }
            _ => None,
        })
        .expect("query result routed");
    assert_eq!(result.text, "delegated answer");
    h.shutdown().expect("shutdown");
}

/// A side agent that receives `agent.message` while its original turn is in
/// flight must process that internal message before teardown. Otherwise the
/// `PromptOriginator::Extension` completion path removes the side conversation
/// and drops the queued delivery.
#[test]
fn side_agent_drains_agent_message_before_extension_teardown() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-message".to_owned(),
            agent_id: "test-agent-q-message".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let (side_spid, side_cid) = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default").then(|| (spid.clone(), prompt_cid.clone()))
        })
        .expect("side prompt id");
    let recipient_id = h
        .conversations
        .get(&side_cid)
        .and_then(|conv| conv.agent_id.clone())
        .expect("side agent id");

    h.publish_event(
        Some(HARNESS_CONNECTION_ID),
        Event::AgentMessage(tau_proto::AgentMessage {
            session_id: "s1".into(),
            sender_id: "manager".to_owned(),
            recipient_id,
            message: "please include this".to_owned(),
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "initial answer".to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q-message".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side first response");

    assert!(
        h.conversations.contains_key(&side_cid),
        "side conversation must stay alive to process queued agent.message"
    );
    assert!(
        delegate_events
            .lock()
            .expect("delegate events")
            .iter()
            .all(|routed| !matches!(routed.frame, Frame::Event(Event::StartAgentResult(_)))),
        "start result must wait until the message turn completes"
    );
    let message_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid == &side_cid && spid != &side_spid).then_some(spid.clone())
        })
        .expect("message prompt dispatched");
    let prompt = read_prompt_created(&h, &message_spid);
    let serialized = serde_json::to_string(&prompt.context_items).expect("json");
    assert!(serialized.contains("please include this"));

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: message_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "final answer".to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q-message".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side message response");

    assert!(
        !h.conversations.contains_key(&side_cid),
        "side conversation tears down after message turn"
    );
    let events = delegate_events.lock().expect("delegate events");
    let result = events
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Event(Event::StartAgentResult(result)) if result.query_id == "q-message" => {
                Some(result)
            }
            _ => None,
        })
        .expect("query result routed");
    assert_eq!(result.text, "final answer");
    h.shutdown().expect("shutdown");
}

/// A tool-backed `StartAgentRequest` (`tool_call_id: Some(...)`) is the
/// `delegate` path: it dispatches *while the parent's tool call is
/// still in flight*, so the parent conv's tip is a `ToolUse` block
/// with no matching `ToolResult` yet. The side conv must therefore
/// fork off the tree root with `head: None`, NOT inherit the
/// parent's branch — otherwise (a) the assembled prompt would carry
/// an orphan `ToolUse` block (provider 400s on unmatched tool_use),
/// and (b) the sub-agent would see the user's framing and might
/// recursively re-delegate the same task. (Contrast with the
/// non-tool path, where `tool_call_id: None` deliberately inherits
/// the parent — see `non_tool_start_agent_request_inherits_parent_branch`.)
#[test]
fn start_agent_request_during_tool_call_branches_off_unresolved_tool_use() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
    let spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("delegate"),
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
    .expect("tool response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q1".to_owned(),
            agent_id: "test-agent-q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);

    // Tool-backed sub-agents (`tool_call_id: Some(...)`) get a fresh
    // context regardless of whether the parent is mid-tool-call: they
    // see only their own `query.instruction`, never the parent's
    // unresolved `delegate` tool_use (which would be an orphan ToolUse
    // the provider rejects), and never the user's task framing (which
    // would invite recursive re-delegation).
    let saw_orphan_tool_use = prompt
        .context_items
        .iter()
        .any(|item| tool_call_id(item) == Some("delegate-call"));
    assert!(
        !saw_orphan_tool_use,
        "side prompt must not replay the parent's unresolved delegate tool_use"
    );

    let saw_user_framing = prompt.context_items.iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("delegate something"))
        )
    });
    assert!(
        !saw_user_framing,
        "side prompt must NOT inherit the user's task framing — sub-agents start with a fresh context"
    );

    let saw_own_instruction = prompt.context_items.iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item) == Some("side task")
        )
    });
    assert!(
        saw_own_instruction,
        "side prompt should contain the delegated instruction"
    );

    h.shutdown().expect("shutdown");
}

/// A non-tool `StartAgentRequest` (`tool_call_id: None`, e.g.
/// `std-notifications`' idle summary) is **not** a delegate. Its
/// purpose is to summarize what the user just did, so the side conv
/// must inherit the parent conversation's branch — assembling the
/// user's recent UserMessage / AgentMessage history *plus* the new
/// instruction. The whole feature falls back to a useless generic
/// greeting if the model is asked to summarize an empty transcript.
///
/// This is also why we don't strip tools / system prompt for these
/// queries: the side conv's request reuses the parent's cached prefix
/// (system_prompt + tools + full transcript) and adds only the
/// instruction as a delta. Verified here by comparing the assembled
/// prompt to what the parent conv sees.
#[test]
fn non_tool_start_agent_request_inherits_parent_branch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());

    // Drive the user's main conversation through one full
    // user-message → agent-final-response turn so the parent conv has
    // a non-empty history when the idle summary fires.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "find the bug in foo.rs".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "I fixed the off-by-one in foo.rs".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
    .expect("main response");
    let parent_head_before = h.conversations.get(&cid).expect("default conv").head;
    assert!(
        parent_head_before.is_some(),
        "parent conv should have advanced its head after the agent's reply",
    );

    // std-notifications-shaped query: no tool_call_id, just an
    // instruction asking the model to summarize.
    h.handle_start_agent_request(
        "conn-notifications",
        StartAgentRequest {
            query_id: "idle-0".to_owned(),
            agent_id: "test-agent-idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let side_prompt = read_prompt_created(&h, &side_spid);

    // The assembled side prompt must contain the user's original
    // task, the agent's final answer, AND the new instruction — in
    // that order. Without inheritance the side prompt would only
    // hold the instruction and the model would default to a generic
    // "I'm ready for your next task" reply.
    let user_task_present = side_prompt.context_items.iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("find the bug in foo.rs"))
        )
    });
    let agent_answer_present = side_prompt.context_items.iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("I fixed the off-by-one"))
        )
    });
    let instruction_present = side_prompt.context_items.iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item) == Some("Summarize in one sentence.")
        )
    });
    assert!(
        user_task_present,
        "side prompt must inherit the user's original task message: {:?}",
        side_prompt.context_items,
    );
    assert!(
        agent_answer_present,
        "side prompt must inherit the agent's final reply: {:?}",
        side_prompt.context_items,
    );
    assert!(
        instruction_present,
        "side prompt must contain the summarize-instruction itself: {:?}",
        side_prompt.context_items,
    );

    // Tool execution is blocked locally by the harness. The provider
    // request must still keep `tool_choice: Auto` so the side query's
    // non-input fields match the parent conv's cached chain.
    assert_eq!(
        side_prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "non-tool start-agent request must preserve wire tool_choice for cache compatibility",
    );

    // The parent conv's head must not have moved sideways because of
    // the side conv's publish — both convs are now downstream of the
    // parent's previous tip, but the side conv folded onto its own
    // child node.
    let parent_head_after = h.conversations.get(&cid).expect("default conv").head;
    assert_eq!(
        parent_head_before, parent_head_after,
        "side conv's UserMessage must not advance the parent conv's head",
    );

    h.shutdown().expect("shutdown");
}

/// A non-tool start-agent request (idle-summary path) must not execute
/// tools, but it also must not mutate provider-visible request fields
/// to enforce that policy. The side conv inherits the parent's
/// `previous_response_id` so the upstream prompt cache is reused
/// instead of paying for a full transcript replay (~50k tokens per
/// idle summary in real sessions). It must preserve `tool_choice:
/// Auto`; flipping it to `None` changes the wire request and was
/// observed to collapse cache usage to near zero even with a valid
/// `previous_response_id`.
#[test]
fn non_tool_start_agent_request_preserves_chain_anchor_and_tool_choice() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    // Drive one full main-conv turn through the normal dispatch path
    // so `prompt_fingerprints`/`prompt_models` are populated and
    // `handle_provider_response_finished` actually mints the anchor.
    h.submit_user_prompt("s1".into(), "find the bug in foo.rs".to_owned())
        .expect("submit main");
    let main_spid: SessionPromptId = "sp-0".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "I fixed the off-by-one in foo.rs".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_parent".to_owned()),
        ws_pool_delta: None,
    })
    .expect("main response");

    // std-notifications-shaped query — `tool_call_id: None` triggers
    // the `tool_choice: None` branch in `send_prompt_to_agent_for`.
    h.handle_start_agent_request(
        "conn-notifications",
        StartAgentRequest {
            query_id: "idle-0".to_owned(),
            agent_id: "test-agent-idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let side_prompt = read_prompt_created(&h, &side_spid);

    assert_eq!(
        side_prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "idle-summary query must preserve the parent's wire tool_choice; the harness enforces no-tool execution locally",
    );
    assert!(
        side_prompt.share_user_cache_key,
        "idle-summary side conv must opt out of the extension cache-key split — \
         otherwise it cold-starts a separate cache bucket from the user's prefix \
         and the whole point of sharing the warm prefix is lost",
    );
    let prev = side_prompt.previous_response_candidate.as_ref().expect(
        "idle-summary side conv must inherit parent's chain anchor — \
         the wire request stays cache-compatible with the parent",
    );
    assert_eq!(prev.provider_response_id, "resp_parent");
}

/// Counterpart to `non_tool_start_agent_request_inherits_parent_branch`.
/// The harness picks `tool_choice` per conversation in
/// `send_prompt_to_agent_for`; if that discriminator ever
/// over-matches (e.g. flips on `originator.is_extension()` alone),
/// delegate sub-agents would receive `tool_choice: "none"` and be
/// unable to call any tool — silently turning every delegated task
/// into a one-shot text response. Asserts the inverse leg: when
/// `tool_call_id: Some(...)`, `ToolChoice::Auto` is preserved.
#[test]
fn delegate_start_agent_request_keeps_tool_choice_auto() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "go".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("delegate"),
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
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q1".to_owned(),
            agent_id: "test-agent-q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);
    assert_eq!(
        prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "delegated sub-agent must keep tool access (ToolChoice::Auto)",
    );
    assert!(
        !prompt.share_user_cache_key,
        "delegate sub-agents must keep the per-extension cache-key split — \
         parallel fan-out would otherwise push the user's bucket past \
         OpenAI's 15-RPM-per-(prefix, key) routing guideline",
    );

    h.shutdown().expect("shutdown");
}

/// Regression for the `tau-agent-bsjr7t` stall: an in-flight
/// non-tool extension side conversation (idle-summary stuck on a
/// usage-limit retry) must be preempted as soon as the user submits
/// a fresh prompt. Otherwise the agent's single prompt slot keeps
/// burning backoff retries on the side conv while the user waits.
#[test]
fn user_prompt_preempts_in_flight_non_tool_ext_side_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    // Seed an in-flight idle-summary side conv with a previously
    // dispatched spid that's notionally still being retried by the
    // agent.
    h.handle_start_agent_request(
        "conn-notifications",
        StartAgentRequest {
            query_id: "idle-0".to_owned(),
            agent_id: "test-agent-idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let (side_cid, side_spid) = h
        .prompt_conversations
        .iter()
        .find(|(_, prompt_cid)| prompt_cid.as_str() != "default")
        .map(|(spid, cid)| (cid.clone(), spid.clone()))
        .expect("side conv must exist");
    let side_conv = h.conversations.get(&side_cid).expect("side conv present");
    assert_eq!(
        side_conv.in_flight_prompt.as_ref(),
        Some(&side_spid),
        "sanity: side conv is mid-flight before user submits",
    );

    // User submits a real prompt — the harness must preempt the
    // side conv (cancel it, free the agent slot) before queueing or
    // dispatching the user's turn.
    h.submit_user_prompt("s1".into(), "interrupting prompt".to_owned())
        .expect("submit user");

    let side_conv = h
        .conversations
        .get(&side_cid)
        .expect("side conv still tracked");
    assert!(
        side_conv.in_flight_prompt.is_none(),
        "user prompt must clear the side conv's in-flight spid so the agent's \
         prompt slot is free; still set to {:?}",
        side_conv.in_flight_prompt,
    );
    assert!(
        h.canceled_prompts.contains(&side_spid),
        "side conv's spid must be marked canceled so a late response is dropped",
    );
    assert!(
        !h.prompt_conversations.contains_key(&side_spid),
        "side conv's spid must be unrouted so the agent's eventual abort \
         doesn't try to publish a finished event into a stale slot",
    );
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::SessionPromptTerminated(terminated)
                if terminated.session_prompt_id.as_str() == side_spid.as_str()
                    && terminated.reason == tau_proto::SessionPromptTerminationReason::Canceled
        )),
        "preempted side prompt must publish a terminal lifecycle event",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a sub-agent's `Shared` tool call must not be gated by the
/// parent's still-in-flight `Exclusive` `delegate` call. The parent's
/// delegate only resolves once the sub-agent's tools have run, so a
/// global execution-mode gate produces a self-deadlock — the main
/// symptom we hit in `tau-agent-m2dpw4`'s event log.
#[test]
fn side_conversation_shared_tool_dispatches_through_parent_exclusive_delegate() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
    let websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: tau_proto::ToolName::new("websearch"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );

    // Main agent issues `delegate`, putting an Exclusive call in flight
    // on the default conversation.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("delegate"),
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
    .expect("main response");

    // Delegate extension turns it into an StartAgentRequest; the harness
    // spawns a side conversation and dispatches its prompt.
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q1".to_owned(),
            agent_id: "test-agent-q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    // Sub-agent now responds with a Shared `websearch` call. Without
    // per-conversation gating this would queue forever behind the
    // parent's still-in-flight Exclusive `delegate`.
    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "websearch-call".into(),
            name: tau_proto::ToolName::new("websearch"),
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
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response");

    // The Shared call must have been accepted for the websearch
    // extension. The harness broadcasts `ToolStarted`; the
    // subscribed provider sees that event and starts the tool.
    let saw_routed = websearch_events.lock().expect("ws").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolStarted(invoke)) if invoke.call_id.as_str() == "websearch-call"
        )
    });
    assert!(
        saw_routed,
        "side conversation's Shared tool must dispatch despite parent's in-flight Exclusive delegate"
    );
    assert_eq!(
        h.tool_turn.pending_len(),
        0,
        "no entries should be left queued"
    );

    h.shutdown().expect("shutdown");
}

/// Background tool completion must survive side-conversation teardown. A
/// sub-agent can finish after its foreground receives the synthetic background
/// placeholder while the real tool is still running; the late completion prompt
/// is transferred to the live parent agent conversation instead of being lost
/// with the removed side conversation.
#[test]
fn background_completion_from_removed_side_conversation_queues_on_parent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-delegate");
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
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    let _ = connect_test_tool(&mut h, "conn-slow");
    h.registry.register(
        "conn-slow",
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
        },
    );

    let parent_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &parent_cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_conversation(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate slow work".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: ToolName::new("delegate"),
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
    .expect("main delegate call");

    let mut query = ext_query("q-bg", ToolExecutionMode::Shared);
    query.tool_call_id = Some("delegate-call".into());
    h.handle_start_agent_request("conn-delegate", query)
        .expect("side query");
    let side_cid = ext_query_cid(&h, "q-bg").expect("side conversation");
    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call".into(),
            name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-bg".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side tool call");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "slow-call"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "slow-call"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));

    let followup_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side follow-up prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: followup_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "side answer".to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-bg".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish side conversation");
    assert!(!h.conversations.contains_key(&side_cid));
    assert_eq!(h.tool_conversations.get("slow-call"), Some(&parent_cid));

    h.handle_extension_event_inner(
        "conn-slow",
        Event::ToolResult(ToolResult {
            call_id: "slow-call".into(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("real output".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("late tool result");

    assert!(event_log_contains(&h, "conn-slow", |event| matches!(
        event,
        Event::ToolBackgroundResult(result)
            if result.call_id.as_str() == "slow-call"
                && matches!(&result.result, CborValue::Text(text) if text == "real output")
    )));
    let parent = h
        .conversations
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(parent.pending_prompts.iter().any(|prompt| prompt.text
        == background_completion_prompt(&"slow-call".into())
        && prompt.is_internal()));

    h.shutdown().expect("shutdown");
}

/// Canceled side conversations must not transfer their inner background tools
/// to the parent. Otherwise a canceled delegate can leak an inner shell
/// completion prompt and make that inner call waitable in the parent
/// conversation.
#[test]
fn canceled_side_conversation_drops_inner_background_completion() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-delegate");
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
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    let _ = connect_test_tool(&mut h, "conn-slow");
    h.registry.register(
        "conn-slow",
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
        },
    );

    let parent_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main-cancel".into();
    seed_agent_thinking(&mut h, &parent_cid, "sp-main-cancel");
    h.prompt_conversations
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_conversation(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate slow work".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call-cancel".into(),
            name: ToolName::new("delegate"),
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
    .expect("main delegate call");

    let mut query = ext_query("q-bg-cancel", ToolExecutionMode::Shared);
    query.tool_call_id = Some("delegate-call-cancel".into());
    h.handle_start_agent_request("conn-delegate", query)
        .expect("side query");
    let side_cid = ext_query_cid(&h, "q-bg-cancel").expect("side conversation");
    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call-cancel".into(),
            name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-bg-cancel".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side tool call");

    h.cancel_start_agent_request("q-bg-cancel", &"delegate-call-cancel".into(), false)
        .expect("cancel delegate");
    assert!(!h.conversations.contains_key(&side_cid));
    assert!(!h.tool_conversations.contains_key("slow-call-cancel"));

    h.handle_extension_event_inner(
        "conn-slow",
        Event::ToolResult(ToolResult {
            call_id: "slow-call-cancel".into(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("real output".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("late tool result is ignored");

    assert!(!event_log_contains(&h, "conn-slow", |event| matches!(
        event,
        Event::ToolBackgroundResult(result) if result.call_id.as_str() == "slow-call-cancel"
    )));
    let parent = h
        .conversations
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(!parent.pending_prompts.iter().any(|prompt| prompt.text
        == background_completion_prompt(&"slow-call-cancel".into())
        && prompt.is_internal()));

    h.shutdown().expect("shutdown");
}

#[test]
fn background_notification_suppression_keeps_error_event_but_skips_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-fail");
    h.registry.register(
        "conn-fail",
        ToolSpec {
            name: ToolName::new("fail"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
        },
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-bg-error".into();
    seed_agent_thinking(&mut h, &cid, "sp-bg-error");
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run fail".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "fail-call".into(),
            name: ToolName::new("fail"),
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
    .expect("background tool call");

    h.suppress_background_completion_prompt("fail-call".into());
    h.handle_extension_event_inner(
        "conn-fail",
        Event::ToolError(tau_proto::ToolError {
            call_id: "fail-call".into(),
            tool_name: ToolName::new("fail"),
            tool_type: tau_proto::ToolType::Function,
            message: "late failure".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("late tool error");

    assert!(event_log_contains(&h, "conn-fail", |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "fail-call" && error.message == "late failure"
    )));
    assert!(!event_log_contains(&h, "conn-fail", |event| matches!(
        event,
        Event::ToolError(error) if error.call_id.as_str() == "fail-call"
    )));
    let conv = h
        .conversations
        .get(&cid)
        .expect("conversation remains live");
    assert!(
        conv.pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&"fail-call".into()))
    );

    h.shutdown().expect("shutdown");
}

/// If a wait is interrupted before the background call finishes, unsuppressing
/// first should let the later completion queue the normal internal prompt.
#[test]
fn background_notification_unsuppress_before_completion_allows_later_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-unsuppress-before".into();

    h.suppress_background_completion_prompt(call_id.clone());
    h.unsuppress_background_completion_prompt(call_id.clone());

    h.conversations
        .get_mut(&cid)
        .expect("default conversation remains live")
        .turn_state = ConversationTurnState::ToolsRunning {
        remaining_calls: Vec::new(),
    };
    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);

    let conv = h
        .conversations
        .get(&cid)
        .expect("default conversation remains live");
    assert!(conv.pending_prompts.iter().any(|prompt| {
        prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
    }));

    h.shutdown().expect("shutdown");
}

/// If the real background completion arrives while suppressed, unsuppressing
/// later should restore the completion prompt from the recorded target map.
#[test]
fn background_notification_unsuppress_after_suppressed_completion_queues_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-unsuppress-after".into();

    h.suppress_background_completion_prompt(call_id.clone());
    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);
    assert!(
        h.conversations
            .get(&cid)
            .expect("default conversation remains live")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.conversations
        .get_mut(&cid)
        .expect("default conversation remains live")
        .turn_state = ConversationTurnState::ToolsRunning {
        remaining_calls: Vec::new(),
    };
    h.unsuppress_background_completion_prompt(call_id.clone());

    let conv = h
        .conversations
        .get(&cid)
        .expect("default conversation remains live");
    assert!(conv.pending_prompts.iter().any(|prompt| {
        prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
    }));

    h.shutdown().expect("shutdown");
}

/// Completed background calls remain in the target map so repeated wait cycles
/// can remove and then re-add the queued internal completion prompt.
#[test]
fn background_notification_repeated_suppress_unsuppress_after_completion_requeues_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg-repeat".into();

    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);
    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.conversations
            .get(&cid)
            .expect("default conversation remains live")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.unsuppress_background_completion_prompt(call_id.clone());
    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.conversations
            .get(&cid)
            .expect("default conversation remains live")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.unsuppress_background_completion_prompt(call_id.clone());
    let conv = h
        .conversations
        .get(&cid)
        .expect("default conversation remains live");
    let prompt_count = conv
        .pending_prompts
        .iter()
        .filter(|prompt| prompt.text == background_completion_prompt(&call_id))
        .count();
    assert_eq!(prompt_count, 1);

    h.shutdown().expect("shutdown");
}

/// Late progress for a backgrounded tool must not be published. The foreground
/// tool result has already closed the visible tool block, so orphan progress
/// would render as confusing standalone text like `shell: running shell
/// command`.
#[test]
fn backgrounded_tool_progress_is_not_published() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-slow");
    h.registry.register(
        "conn-slow",
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
        },
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-bg-progress".into();
    seed_agent_thinking(&mut h, &cid, "sp-bg-progress");
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run slow".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call".into(),
            name: ToolName::new("slow"),
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
    .expect("background tool call");

    h.handle_extension_event_inner(
        "conn-slow",
        Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "slow-call".into(),
            tool_name: ToolName::new("slow"),
            message: Some("running shell command".to_owned()),
            progress: None,
        }),
    )
    .expect("late progress");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolProgress(progress) if progress.call_id.as_str() == "slow-call"
    )));

    h.shutdown().expect("shutdown");
}

/// Suppression can arrive after a background completion prompt was queued but
/// before the agent saw it; in that case the queued internal prompt is removed.
#[test]
fn background_notification_suppression_removes_queued_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let call_id: ToolCallId = "bg".into();

    h.conversations
        .get_mut(&cid)
        .expect("default conversation exists")
        .pending_prompts
        .push_back(PendingPrompt::internal(background_completion_prompt(
            &call_id,
        )));
    assert!(
        h.conversations
            .get(&cid)
            .expect("default conversation exists")
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == background_completion_prompt(&call_id))
    );

    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.conversations
            .get(&cid)
            .expect("default conversation exists")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn shared_start_agent_requests_start_concurrently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");

    h.handle_start_agent_request("conn-a", ext_query("q-a", ToolExecutionMode::Shared))
        .expect("query a");
    h.handle_start_agent_request("conn-b", ext_query("q-b", ToolExecutionMode::Shared))
        .expect("query b");

    assert!(ext_query_cid(&h, "q-a").is_some());
    assert!(ext_query_cid(&h, "q-b").is_some());
    assert!(h.pending_start_agent_requests.is_empty());
    assert_eq!(h.active_start_agent_requests.len(), 2);

    h.shutdown().expect("shutdown");
}

/// Update sub-agent queries may overlap with shared research, but only one
/// update lane runs at a time. A blocked update also acts as a FIFO barrier so
/// later independent shared work cannot jump ahead and starve updates.
#[test]
fn update_start_agent_request_overlaps_with_shared_and_blocks_later_fifo_jump() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");
    let _ = connect_test_tool(&mut h, "conn-c");
    let _ = connect_test_tool(&mut h, "conn-d");

    h.handle_start_agent_request(
        "conn-a",
        ext_query("q-update-active", ToolExecutionMode::Update),
    )
    .expect("active update query");
    let active_update_cid = ext_query_cid(&h, "q-update-active").expect("active update started");
    h.handle_start_agent_request(
        "conn-b",
        ext_query("q-shared-active", ToolExecutionMode::Shared),
    )
    .expect("active shared query");
    h.handle_start_agent_request(
        "conn-c",
        ext_query("q-update-blocked", ToolExecutionMode::Update),
    )
    .expect("blocked update query");
    h.handle_start_agent_request(
        "conn-d",
        ext_query("q-later-shared", ToolExecutionMode::Shared),
    )
    .expect("later shared query");

    assert!(ext_query_cid(&h, "q-shared-active").is_some());
    assert!(ext_query_cid(&h, "q-update-blocked").is_none());
    assert!(ext_query_cid(&h, "q-later-shared").is_none());
    assert_eq!(h.pending_start_agent_requests.len(), 2);

    finish_ext_query(&mut h, &active_update_cid, "q-update-active");

    assert!(ext_query_cid(&h, "q-update-blocked").is_some());
    assert!(ext_query_cid(&h, "q-later-shared").is_some());
    assert!(h.pending_start_agent_requests.is_empty());

    h.shutdown().expect("shutdown");
}

/// An Exclusive sub-agent is process-global for independent sub-agent work: it
/// waits for all incompatible side conversations and then blocks later shared
/// or exclusive `StartAgentRequest`s until its result is routed back.
#[test]
fn exclusive_start_agent_request_blocks_independent_queries_globally() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");
    let _ = connect_test_tool(&mut h, "conn-c");

    h.handle_start_agent_request(
        "conn-a",
        ext_query("q-exclusive", ToolExecutionMode::Exclusive),
    )
    .expect("exclusive query");
    let exclusive_cid = ext_query_cid(&h, "q-exclusive").expect("exclusive started");
    h.handle_start_agent_request("conn-b", ext_query("q-shared", ToolExecutionMode::Shared))
        .expect("shared query");
    h.handle_start_agent_request(
        "conn-c",
        ext_query("q-exclusive-2", ToolExecutionMode::Exclusive),
    )
    .expect("second exclusive query");

    assert!(ext_query_cid(&h, "q-shared").is_none());
    assert!(ext_query_cid(&h, "q-exclusive-2").is_none());
    assert_eq!(h.pending_start_agent_requests.len(), 2);

    finish_ext_query(&mut h, &exclusive_cid, "q-exclusive");

    assert!(ext_query_cid(&h, "q-exclusive").is_none());
    assert!(ext_query_cid(&h, "q-shared").is_some());
    assert!(
        ext_query_cid(&h, "q-exclusive-2").is_none(),
        "second exclusive must wait for the shared query that was ahead of it"
    );
    assert_eq!(h.pending_start_agent_requests.len(), 1);

    h.shutdown().expect("shutdown");
}

/// FIFO matters for global sub-agent scheduling: once an Exclusive is queued
/// behind active work, later independent Shared queries must not jump it and
/// starve exclusive work forever.
#[test]
fn queued_exclusive_prevents_later_shared_from_jumping_fifo() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");
    let _ = connect_test_tool(&mut h, "conn-c");

    h.handle_start_agent_request("conn-a", ext_query("q-active", ToolExecutionMode::Shared))
        .expect("active shared query");
    let active_cid = ext_query_cid(&h, "q-active").expect("active shared started");
    h.handle_start_agent_request(
        "conn-b",
        ext_query("q-exclusive", ToolExecutionMode::Exclusive),
    )
    .expect("queued exclusive query");
    h.handle_start_agent_request(
        "conn-c",
        ext_query("q-later-shared", ToolExecutionMode::Shared),
    )
    .expect("later shared query");

    assert!(ext_query_cid(&h, "q-exclusive").is_none());
    assert!(ext_query_cid(&h, "q-later-shared").is_none());
    assert_eq!(h.pending_start_agent_requests.len(), 2);

    finish_ext_query(&mut h, &active_cid, "q-active");

    assert!(ext_query_cid(&h, "q-exclusive").is_some());
    assert!(
        ext_query_cid(&h, "q-later-shared").is_none(),
        "later shared query must remain queued behind the exclusive"
    );
    assert_eq!(h.pending_start_agent_requests.len(), 1);

    h.shutdown().expect("shutdown");
}

/// Nested delegates inside an active Exclusive sub-agent are reentrant: the
/// harness treats them as part of the exclusive subtree instead of making the
/// parent wait on itself forever.
#[test]
fn nested_start_agent_request_under_active_exclusive_is_allowed() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        "conn-delegate",
        ext_query("q-outer", ToolExecutionMode::Exclusive),
    )
    .expect("outer query");
    let outer_cid = ext_query_cid(&h, "q-outer").expect("outer started");

    h.tool_conversations
        .insert("nested-call".into(), outer_cid.clone());
    let mut nested = ext_query("q-nested", ToolExecutionMode::Shared);
    nested.tool_call_id = Some("nested-call".into());
    nested.task_name = Some("nested".to_owned());
    h.handle_start_agent_request("conn-delegate", nested)
        .expect("nested query");

    let nested_cid = ext_query_cid(&h, "q-nested").expect("nested started");
    assert_ne!(outer_cid, nested_cid);
    assert!(h.pending_start_agent_requests.is_empty());
    assert_eq!(h.active_start_agent_requests.len(), 2);

    h.shutdown().expect("shutdown");
}

/// Unlike exclusive sub-agent work, update sub-agents only overlap with shared
/// descendants. A nested update must wait for the parent update to finish so
/// two update lanes are never active together.
#[test]
fn nested_update_start_agent_request_under_active_update_waits() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        "conn-delegate",
        ext_query("q-outer", ToolExecutionMode::Update),
    )
    .expect("outer query");
    let outer_cid = ext_query_cid(&h, "q-outer").expect("outer started");

    h.tool_conversations
        .insert("nested-call".into(), outer_cid.clone());
    let mut nested = ext_query("q-nested", ToolExecutionMode::Update);
    nested.tool_call_id = Some("nested-call".into());
    nested.task_name = Some("nested".to_owned());
    h.handle_start_agent_request("conn-delegate", nested)
        .expect("nested query");

    assert!(ext_query_cid(&h, "q-nested").is_none());
    assert_eq!(h.pending_start_agent_requests.len(), 1);

    finish_ext_query(&mut h, &outer_cid, "q-outer");
    assert!(ext_query_cid(&h, "q-nested").is_some());
    assert!(h.pending_start_agent_requests.is_empty());

    h.shutdown().expect("shutdown");
}

/// A wait that is already blocked on a tool call must be released even when the
/// terminal event is a harness-synthesized routing error instead of a provider
/// response. Otherwise `wait` can hang forever after unavailable-tool paths.
#[test]
fn wait_resolves_on_synthetic_tool_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let target_call_id: ToolCallId = "target-call".into();

    h.tool_conversations
        .insert(target_call_id.clone(), cid.clone());
    h.pending_tools.insert(
        target_call_id.clone(),
        PendingTool {
            name: ToolName::new("missing"),
            internal_name: ToolName::new("missing"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.record_wait_tool_request(&target_call_id);

    let wait_call = AgentToolCall {
        id: "wait-call".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
        display: None,
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");

    let missing_message = unavailable_tool_error_message(&ToolName::new("missing"));
    h.publish_terminal_tool_error(
        Some(&cid),
        None,
        tau_proto::ToolError {
            call_id: target_call_id,
            tool_name: ToolName::new("missing"),
            tool_type: tau_proto::ToolType::Function,
            message: missing_message.clone(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
    );

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-call"
                && error.message == missing_message
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: `wait` is harness-owned and publishes its answer inline, but the
/// answer still must be folded as a provider-terminal tool output. Otherwise
/// the next full replay contains the `wait` ToolCall without a matching
/// ToolResult, which OpenAI rejects with `No tool output found for function
/// call …`.
#[test]
fn wait_tool_reply_is_folded_into_followup_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = h.default_conversation_id.clone();
    append_user_message_via_event(&mut h, "s1", "wait on missing call");
    seed_agent_thinking(&mut h, &cid, "sp-wait");
    let spid: SessionPromptId = "sp-wait".into();
    h.prompt_conversations.insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-call".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("missing-target".to_owned()),
            )]),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("wait response");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error) if error.call_id.as_str() == "wait-call"
    )));
    let followup_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(prompt_id, prompt_cid)| {
            (prompt_id != &spid && prompt_cid == &cid).then_some(prompt_id.clone())
        })
        .expect("follow-up prompt id");
    let prompt = read_prompt_created(&h, &followup_spid);
    let tool_uses: Vec<&str> = prompt
        .context_items
        .iter()
        .filter_map(tool_call_id)
        .collect();
    let tool_results: Vec<&str> = prompt
        .context_items
        .iter()
        .filter_map(tool_result_id)
        .collect();

    assert!(
        tool_uses.contains(&"wait-call"),
        "follow-up prompt must include the wait ToolCall; got: {tool_uses:?}",
    );
    assert!(
        tool_results.contains(&"wait-call"),
        "follow-up prompt must include the matching wait ToolResult; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: the delegate tool's advertised `execution_mode` argument
/// controls sub-agent scheduling, not parent-conversation tool scheduling.
#[test]
fn delegate_parent_tool_scheduling_ignores_delegate_execution_mode_argument() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

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
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );

    let explicit_call = AgentToolCall {
        id: "explicit".to_owned().into(),
        name: ToolName::new("delegate"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("execution_mode".to_owned()),
            CborValue::Text("exclusive".to_owned()),
        )]),
        display: None,
    };
    assert_eq!(
        h.resolve_tool_execution_mode_for_call(&explicit_call),
        ToolExecutionMode::Shared,
        "delegate execution_mode affects the emitted StartAgentRequest, not the parent tool invocation"
    );

    h.shutdown().expect("shutdown");
}

/// Exclusive tool serialization is scoped to the owning conversation,
/// not process-global. Two independent sub-agents may both need to run
/// exclusive work; making them wait on each other would unnecessarily
/// serialize otherwise unrelated side tasks and can deadlock nested
/// delegate workflows that depend on sub-agent progress.
#[test]
fn exclusive_tools_in_distinct_side_conversations_dispatch_concurrently() {
    use tau_proto::CborValue;

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
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
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    let _ = connect_test_tool(&mut h, "conn-mutate");
    h.registry.register(
        "conn-mutate",
        ToolSpec {
            name: ToolName::new("mutate"),
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

    // The parent uses shared delegates only to create two realistic
    // side conversations concurrently. The assertion below is about
    // the exclusive tools owned by those distinct side conversations.
    let parent_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &parent_cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_conversation(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "fan out".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let shared_args = CborValue::Map(vec![(
        CborValue::Text("execution_mode".to_owned()),
        CborValue::Text("shared".to_owned()),
    )]);
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-A".into(),
                name: ToolName::new("delegate"),
                tool_type: tau_proto::ToolType::Function,
                arguments: shared_args.clone(),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-B".into(),
                name: ToolName::new("delegate"),
                tool_type: tau_proto::ToolType::Function,
                arguments: shared_args,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-A".to_owned(),
            agent_id: "test-agent-q-A".to_owned(),
            instruction: "side task A".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-A".into()),
            task_name: Some("A".to_owned()),
        },
    )
    .expect("query A");
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-B".to_owned(),
            agent_id: "test-agent-q-B".to_owned(),
            instruction: "side task B".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-B".into()),
            task_name: Some("B".to_owned()),
        },
    )
    .expect("query B");

    let cid_a = h
        .conversations
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-A"
            )
            .then_some(cid.clone())
        })
        .expect("conversation A");
    let cid_b = h
        .conversations
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-B"
            )
            .then_some(cid.clone())
        })
        .expect("conversation B");
    assert_ne!(cid_a, cid_b, "side conversations must be distinct");

    let spid_a = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid_a).then_some(spid.clone()))
        .expect("prompt A");
    let spid_b = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid_b).then_some(spid.clone()))
        .expect("prompt B");

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid_a,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "mut-A".into(),
            name: ToolName::new("mutate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-A".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response A");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid_b,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "mut-B".into(),
            name: ToolName::new("mutate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-B".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response B");

    let mut_a_id: ToolCallId = "mut-A".to_owned().into();
    let mut_b_id: ToolCallId = "mut-B".to_owned().into();
    assert_eq!(
        h.tool_turn.in_flight_mode(&mut_a_id),
        Some(&ToolExecutionMode::Exclusive),
        "conversation A's exclusive call should be in flight",
    );
    assert_eq!(
        h.tool_turn.in_flight_mode(&mut_b_id),
        Some(&ToolExecutionMode::Exclusive),
        "conversation B's exclusive call should be in flight too",
    );
    assert_eq!(h.tool_conversations.get("mut-A"), Some(&cid_a));
    assert_eq!(h.tool_conversations.get("mut-B"), Some(&cid_b));
    assert_ne!(
        h.tool_conversations.get("mut-A"),
        h.tool_conversations.get("mut-B"),
        "exclusive calls must be attributed to different dispatch scopes",
    );
    assert_eq!(
        h.tool_turn.pending_len(),
        0,
        "cross-conversation Exclusive calls should not queue behind each other",
    );

    h.shutdown().expect("shutdown");
}

/// Sub-agent state changes (tool start, response usage, tool finish)
/// must surface to the user as `DelegateProgress` events keyed on the
/// parent's `delegate` tool call_id. The CLI uses these to repaint
/// the running tool block as `delegate [task] %a/b #… …`.
#[test]
fn delegate_emits_progress_as_sub_agent_makes_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
    let _websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: tau_proto::ToolName::new("websearch"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );

    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("delegate"),
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
    .expect("main response");

    let sink = collect_event_sink(&mut h);
    let input_stats = tau_proto::ToolDisplayStats::for_text("prompt\nbody");
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q1".to_owned(),
            agent_id: "test-agent-q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Update,
            input_stats,
            tool_call_id: Some("delegate-call".into()),
            task_name: Some("look it up".to_owned()),
        },
    )
    .expect("query");

    // First snapshot: side conversation just spawned, sub-agent has
    // not yet acted. Counters at zero, no context info.
    let initial = pop_delegate_progress(&sink, "delegate-call")
        .expect("initial DelegateProgress on side conv spawn");
    assert_eq!(initial.task_name, "look it up");
    assert_eq!(initial.role.as_deref(), Some("senior-engineer"));
    assert_eq!(initial.execution_mode, Some(ToolExecutionMode::Update));
    assert_eq!(initial.tools_in_flight, 0);
    assert_eq!(initial.tools_total, 0);
    assert_delegate_tools_counter(&initial, Some(0), Some(0));
    assert_delegate_input_stats(&initial, input_stats);
    assert_delegate_counter_order(&initial, &["tools"]);

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "websearch-call".into(),
            name: tau_proto::ToolName::new("websearch"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: match (Some(1234), None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response");

    // After the side response finishes (which carries usage) and the
    // sub-agent's tool starts dispatching, we should observe at
    // least one `DelegateProgress` reflecting the new state. The
    // exact emit count is not load-bearing — the *latest* snapshot
    // is what the UI renders.
    let latest = drain_delegate_progress(&sink, "delegate-call")
        .pop()
        .expect("at least one DelegateProgress after side response");
    assert_eq!(latest.task_name, "look it up");
    assert_eq!(latest.role.as_deref(), Some("senior-engineer"));
    assert_eq!(latest.execution_mode, Some(ToolExecutionMode::Update));
    assert_eq!(latest.tools_in_flight, 1, "websearch is in flight");
    assert_eq!(latest.tools_total, 1, "websearch counts toward total");
    assert_delegate_tools_counter(&latest, Some(0), Some(1));
    assert_eq!(latest.ctx_input_tokens, Some(1234));
    assert_delegate_ctx_counter(&latest, Some(1234), Some(128_000));
    // Regression coverage for the live delegate line: renderers preserve
    // progress_counters order, so tools must precede context in the UI.
    assert_delegate_counter_order(&latest, &["tools", "ctx"]);
    assert_eq!(h.current_session_state.context_input_tokens, None);

    // Complete the sub-agent's tool — counters should drop and a
    // fresh progress event should show 0 in flight, 1 total.
    h.handle_extension_event(
        "conn-websearch",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "websearch-call".into(),
            tool_name: tau_proto::ToolName::new("websearch"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("fake result".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("ws result");
    let after_complete = drain_delegate_progress(&sink, "delegate-call")
        .pop()
        .expect("DelegateProgress after sub-tool completion");
    assert_eq!(
        after_complete.execution_mode,
        Some(ToolExecutionMode::Update)
    );
    assert_eq!(after_complete.tools_in_flight, 0);
    assert_eq!(after_complete.tools_total, 1);
    assert_delegate_tools_counter(&after_complete, Some(1), Some(1));

    h.shutdown().expect("shutdown");
}

/// A backgrounded tool inside a delegate must clean up like a normal late
/// background error when its provider disconnects. Otherwise the delegate UI
/// can stay stuck at one running tool and a suppressed completion prompt cannot
/// be restored when `wait` is interrupted.
#[test]
fn provider_disconnect_for_backgrounded_delegate_tool_updates_progress_and_target() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
    let _websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: tau_proto::ToolName::new("websearch"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
        },
    );

    let parent_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &parent_cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_conversation(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("delegate"),
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
    .expect("main response");

    let sink = collect_event_sink(&mut h);
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-disconnect".to_owned(),
            agent_id: "test-agent-q-disconnect".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: Some("look it up".to_owned()),
        },
    )
    .expect("query");
    drain_delegate_progress(&sink, "delegate-call");

    let side_cid = ext_query_cid(&h, "q-disconnect").expect("side conversation");
    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "websearch-call".into(),
            name: tau_proto::ToolName::new("websearch"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-disconnect".to_owned(),
        },
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response");

    let in_flight = drain_delegate_progress(&sink, "delegate-call")
        .pop()
        .expect("progress after sub-tool starts");
    assert_eq!(in_flight.tools_in_flight, 1);
    assert_eq!(in_flight.tools_total, 1);
    assert_delegate_tools_counter(&in_flight, Some(0), Some(1));
    assert_eq!(
        h.pending_tool_providers
            .get("websearch-call")
            .map(|provider| provider.as_str()),
        Some("conn-websearch")
    );

    let call_id: ToolCallId = "websearch-call".into();
    h.suppress_background_completion_prompt(call_id.clone());
    h.handle_disconnect("conn-websearch");

    let after_disconnect = drain_delegate_progress(&sink, "delegate-call")
        .pop()
        .expect("progress after provider disconnect");
    assert_eq!(after_disconnect.tools_in_flight, 0);
    assert_eq!(after_disconnect.tools_total, 1);
    assert_delegate_tools_counter(&after_disconnect, Some(1), Some(1));
    assert_eq!(
        h.conversations
            .get(&side_cid)
            .expect("side conversation remains live")
            .tools_in_flight,
        0
    );
    assert_eq!(
        h.background_completion_targets.get(&call_id),
        Some(&side_cid)
    );
    assert!(!h.pending_tool_providers.contains_key(&call_id));
    assert!(!h.tool_conversations.contains_key(&call_id));
    let expected = extension_disconnected_background_tool_call_error_message(&call_id);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == call_id.as_str()
                && error.message == expected
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error) if error.call_id.as_str() == call_id.as_str()
    )));
    assert!(
        h.conversations
            .get(&side_cid)
            .expect("side conversation remains live")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.unsuppress_background_completion_prompt(call_id.clone());
    let side = h
        .conversations
        .get(&side_cid)
        .expect("side conversation remains live");
    assert!(side.pending_prompts.iter().any(|prompt| {
        prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
    }));

    h.shutdown().expect("shutdown");
}

/// An explicit `delegate` role must be a real role switch for the sub-agent,
/// not just UI metadata: the prompt uses that role's model, params, prompt, and
/// tool profile.
#[test]
fn delegate_explicit_role_uses_role_model_params_prompt_and_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let engineer_model: tau_proto::ModelId = "test/engineer".into();
    let worker_model: tau_proto::ModelId = "test/worker".into();
    set_available_provider_models(
        &mut h,
        [
            provider_model_info(engineer_model.clone(), 64_000),
            provider_model_info(worker_model.clone(), 256_000),
        ],
    );
    h.selected_role = "engineer".to_owned();
    h.selected_model = Some(engineer_model.clone());
    h.available_roles = std::collections::HashMap::from([
        (
            "engineer".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(engineer_model),
                prompt_fragments: vec![tau_config::settings::RolePromptFragment {
                    name: "engineer.instructions".to_owned(),
                    priority: tau_proto::PromptPriority::new(100),
                    text: tau_proto::PromptContent::new("SMART ROLE PROMPT"),
                }],
                ..Default::default()
            },
        ),
        (
            "worker".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(worker_model.clone()),
                effort: Some(tau_proto::Effort::High),
                verbosity: Some(tau_proto::Verbosity::High),
                thinking_summary: Some(tau_proto::ThinkingSummary::Auto),
                service_tier: Some(tau_proto::ServiceTier::Flex),
                prompt_fragments: vec![
                    tau_config::settings::RolePromptFragment {
                        name: "worker.instructions".to_owned(),
                        priority: tau_proto::PromptPriority::new(100),
                        text: tau_proto::PromptContent::new("WORKER ROLE PROMPT"),
                    },
                    tau_config::settings::RolePromptFragment {
                        name: "worker.extra".to_owned(),
                        priority: tau_proto::PromptPriority::new(200),
                        text: tau_proto::PromptContent::new("WORKER EXTRA PROMPT"),
                    },
                ],
                tools: Some(vec![ToolName::new("allowed_tool")]),
                enable_tools: vec![ToolName::new("enabled_tool")],
                disable_tools: vec![ToolName::new("denied_tool")],
                ..Default::default()
            },
        ),
    ]);
    h.registry.register_with_prompt_fragment(
        "conn-allowed-tool",
        tau_proto::ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("allowed_tool"),
                model_visible_name: None,
                description: Some("allowed".to_owned()),
                parameters: None,
                tool_type: tau_proto::ToolType::Function,
                format: None,
                enabled_by_default: false,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "allowed_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "ALLOWED TOOL PROMPT",
            )),
        },
    );
    h.registry.register_with_prompt_fragment(
        "conn-enabled-tool",
        tau_proto::ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("enabled_tool"),
                model_visible_name: None,
                description: Some("enabled".to_owned()),
                parameters: None,
                tool_type: tau_proto::ToolType::Function,
                format: None,
                enabled_by_default: false,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "enabled_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "ENABLED TOOL PROMPT",
            )),
        },
    );
    h.registry.register_with_prompt_fragment(
        "conn-default-tool",
        tau_proto::ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("default_tool"),
                model_visible_name: None,
                description: Some("default".to_owned()),
                parameters: None,
                tool_type: tau_proto::ToolType::Function,
                format: None,
                enabled_by_default: true,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "default_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "DEFAULT TOOL PROMPT",
            )),
        },
    );
    h.registry.register_with_prompt_fragment(
        "conn-denied-tool",
        tau_proto::ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("denied_tool"),
                model_visible_name: None,
                description: Some("denied".to_owned()),
                parameters: None,
                tool_type: tau_proto::ToolType::Function,
                format: None,
                enabled_by_default: true,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "denied_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "DENIED TOOL PROMPT",
            )),
        },
    );

    let _delegate = connect_test_tool(&mut h, "conn-delegate");
    let sink = collect_event_sink(&mut h);
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-worker".to_owned(),
            agent_id: "test-agent-q-worker".to_owned(),
            instruction: "side task".to_owned(),
            role: Some("worker".to_owned()),
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: Some("use worker".to_owned()),
        },
    )
    .expect("query");

    let progress = pop_delegate_progress(&sink, "delegate-call").expect("initial progress");
    assert_eq!(progress.role.as_deref(), Some("worker"));

    let side_cid = ext_query_cid(&h, "q-worker").expect("side conversation");
    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);

    assert_eq!(prompt.model.as_ref(), Some(&worker_model));
    assert_eq!(prompt.model_params.effort, tau_proto::Effort::High);
    assert_eq!(prompt.model_params.verbosity, tau_proto::Verbosity::High);
    assert_eq!(
        prompt.model_params.thinking_summary,
        tau_proto::ThinkingSummary::Auto
    );
    assert_eq!(
        prompt.model_params.service_tier,
        Some(tau_proto::ServiceTier::Flex)
    );
    assert!(prompt.system_prompt.contains("WORKER ROLE PROMPT"));
    assert!(prompt.system_prompt.contains("WORKER EXTRA PROMPT"));
    assert!(!prompt.system_prompt.contains("SMART ROLE PROMPT"));
    assert!(prompt.system_prompt.contains("ALLOWED TOOL PROMPT"));
    assert!(prompt.system_prompt.contains("ENABLED TOOL PROMPT"));
    assert!(!prompt.system_prompt.contains("DEFAULT TOOL PROMPT"));
    assert!(!prompt.system_prompt.contains("DENIED TOOL PROMPT"));
    assert!(
        prompt
            .tools
            .iter()
            .any(|tool| tool.name.as_str() == "allowed_tool")
    );
    assert!(
        prompt
            .tools
            .iter()
            .any(|tool| tool.name.as_str() == "enabled_tool")
    );
    assert!(
        !prompt
            .tools
            .iter()
            .any(|tool| tool.name.as_str() == "default_tool")
    );
    assert!(
        !prompt
            .tools
            .iter()
            .any(|tool| tool.name.as_str() == "denied_tool")
    );

    h.shutdown().expect("shutdown");
}

fn start_agent_request_error(
    frames: &Arc<Mutex<Vec<RoutedFrame>>>,
    query_id: &str,
) -> Option<String> {
    frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Event(Event::StartAgentResult(result)) if result.query_id == query_id => {
                result.error.clone()
            }
            _ => None,
        })
}

fn configure_delegate_error_roles(h: &mut Harness) {
    let available_model: tau_proto::ModelId = "test/available".into();
    set_available_provider_models(h, [provider_model_info(available_model.clone(), 128_000)]);
    h.available_roles = std::collections::HashMap::from([
        (
            "beta".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(available_model.clone()),
                ..Default::default()
            },
        ),
        (
            "alpha".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(available_model),
                ..Default::default()
            },
        ),
        (
            "offline".to_owned(),
            tau_config::settings::AgentRole {
                model: Some("test/offline".into()),
                ..Default::default()
            },
        ),
    ]);
}

/// Bad delegate roles fail before spawning a side conversation and report the
/// usable role names in sorted order, excluding roles whose model is
/// unavailable.
#[test]
fn delegate_invalid_or_unavailable_role_errors_with_sorted_available_roles() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    configure_delegate_error_roles(&mut h);

    let delegate = connect_test_tool(&mut h, "conn-delegate");
    for (query_id, role, expected_reason) in [
        ("q-missing", "missing", "requested role does not exist"),
        (
            "q-offline",
            "offline",
            "requested role is not backed by an available model",
        ),
    ] {
        h.handle_start_agent_request(
            "conn-delegate",
            StartAgentRequest {
                query_id: query_id.to_owned(),
                agent_id: format!("test-agent-{}", query_id),
                instruction: "side task".to_owned(),
                role: Some(role.to_owned()),
                execution_mode: ToolExecutionMode::Shared,
                input_stats: tau_proto::ToolDisplayStats::default(),
                tool_call_id: Some(format!("delegate-{query_id}").into()),
                task_name: Some(query_id.to_owned()),
            },
        )
        .expect("query");
        let error = start_agent_request_error(&delegate, query_id).expect("query error");
        assert!(error.contains(expected_reason), "got: {error}");
        assert!(
            error.contains("available roles: alpha, beta"),
            "available roles should be sorted and filtered: {error}"
        );
        assert!(
            !error.contains("available roles: alpha, beta, offline"),
            "unavailable role leaked into available role list: {error}"
        );
    }

    h.shutdown().expect("shutdown");
}

/// Omitting `role` on the delegate tool means `senior-engineer`; if that role
/// cannot resolve to an available model, the harness reports that compatibility
/// default as the problem instead of silently falling back to another role.
#[test]
fn delegate_missing_default_senior_engineer_errors_when_unavailable() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    configure_delegate_error_roles(&mut h);

    let delegate = connect_test_tool(&mut h, "conn-delegate");
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-default".to_owned(),
            agent_id: "test-agent-q-default".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: Some("default".to_owned()),
        },
    )
    .expect("query");

    let error = start_agent_request_error(&delegate, "q-default").expect("query error");
    assert!(
        error.contains(
            "delegate requires default role `senior-engineer`, but it is not available: `senior-engineer`"
        ),
        "got: {error}"
    );
    assert!(
        error.contains("available roles: alpha, beta"),
        "got: {error}"
    );
    assert!(ext_query_cid(&h, "q-default").is_none());

    h.shutdown().expect("shutdown");
}

/// Regression: when one side conversation tears down (running
/// `snap_to_default_conversation`) before another's tool result
/// arrives, the result must still fold onto the *originating*
/// conversation's branch. Before this fix, the result landed at
/// `tree.head` (which `snap_to_default` had moved to the parent
/// branch), producing orphan ToolUse blocks in subsequent prompts —
/// the exact `No tool output found for function call …` 400 we hit
/// in `tau-agent-yvxco1`'s log.
#[test]
fn sibling_side_conv_teardown_does_not_misplace_other_side_conv_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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

    // Set up the main agent's turn that emits a single delegate call.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("delegate"),
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
    .expect("main response");

    // Spawn the outer side conversation.
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-outer".to_owned(),
            agent_id: "test-agent-q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    // Have the outer sub-agent emit a *nested* delegate. The harness
    // should issue another StartAgentRequest for it, which we then ack
    // with a fresh side conversation. This is the exact pattern that
    // produced the misplacement: outer side conv runs teardown
    // (snap_to_default) before nested side conv's tool result lands.
    let outer_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: outer_side_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "nested-call".into(),
            name: ToolName::new("delegate"),
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
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-outer".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("outer response");
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-nested".to_owned(),
            agent_id: "test-agent-q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    // Nested sub-agent finishes with a final answer. This triggers
    // side teardown: `snap_to_default_conversation` runs, moving
    // tree.head back to the main branch. The delegate ext then
    // publishes a ToolResult for `nested-call` — which must fold on
    // the *outer* conv's branch (since outer issued nested-call), not
    // wherever tree.head happens to be.
    let nested_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: nested_side_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "nested answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-nested".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("nested final");

    // The delegate extension would route the nested StartAgentResult
    // back as a ToolResult — simulate that here.
    h.handle_extension_event(
        "conn-delegate",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "nested-call".into(),
            tool_name: tau_proto::ToolName::new("delegate"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("nested answer".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("nested tool result");

    // Now re-prompt the outer sub-agent and inspect the assembled
    // messages. The `outer-call` tool_use must NOT appear in the
    // outer sub-agent's branch — the only ToolUse the outer
    // sub-agent should see is its own `nested-call` (with a
    // matching ToolResult).
    let outer_resume_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() == outer_side_cid_str(&h)).then_some(spid.clone())
        })
        .expect("outer resume prompt id");
    let prompt = read_prompt_created(&h, &outer_resume_spid);

    let tool_uses: Vec<String> = prompt
        .context_items
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    let tool_results: Vec<String> = prompt
        .context_items
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "outer sub-agent's prompt must not include the parent's `outer-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_uses.iter().any(|id| id == "nested-call"),
        "outer sub-agent's prompt must include its own `nested-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_results.iter().any(|id| id == "nested-call"),
        "outer sub-agent must see the matching ToolResult for `nested-call`; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: nested extension-agent queries must branch from the
/// conversation that issued the nested tool call. Branching from the
/// default conversation can replay unrelated in-flight ToolUse blocks
/// from the main branch into the nested sub-agent prompt, which OpenAI
/// rejects with `No tool output found for function call …`.
#[test]
fn nested_start_agent_request_branches_from_tool_owner_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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

    let default_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_conversation(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("delegate"),
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
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-outer".to_owned(),
            agent_id: "test-agent-q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("outer query");

    let outer_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: outer_side_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "nested-call".into(),
            name: ToolName::new("delegate"),
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
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-outer".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("outer response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-nested".to_owned(),
            agent_id: "test-agent-q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    let nested_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    let prompt = read_prompt_created(&h, &nested_side_spid);

    let tool_uses: Vec<String> = prompt
        .context_items
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "nested sub-agent's prompt must not include the default branch's unresolved `outer-call`; got: {tool_uses:?}",
    );
    assert!(
        !tool_uses.iter().any(|id| id == "nested-call"),
        "nested sub-agent starts before its parent call has a result, so it must not include `nested-call`; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn completed_side_conversation_tool_result_reprompts_parent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
    let spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("delegate"),
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
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-outer".to_owned(),
            agent_id: "test-agent-q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: side_spid,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "outer answer".to_owned(),
            }],

            phase: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-outer".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side final");

    h.handle_extension_event(
        "conn-delegate",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "outer-call".into(),
            tool_name: tau_proto::ToolName::new("delegate"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("outer answer".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("delegate result");

    let main_resume_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() == "default").then_some(spid.clone()))
        .expect("main resume prompt id");
    let prompt = read_prompt_created(&h, &main_resume_spid);
    let tool_results: Vec<String> = prompt
        .context_items
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert!(
        tool_results.iter().any(|id| id == "outer-call"),
        "parent conversation must be re-prompted with delegate ToolResult; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn recursive_delegate_prompt_contains_only_leaf_instruction() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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

    let default_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_conversation(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "ROOT: ask top delegate to delegate again".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: main_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "top-call".into(),
            name: ToolName::new("delegate"),
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
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-top".to_owned(),
            agent_id: "test-agent-q-top".to_owned(),
            instruction: "TOP: delegate exactly two more subtasks".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("top-call".into()),
            task_name: Some("top".to_owned()),
        },
    )
    .expect("top query");

    let top_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("top prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: top_spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "leaf-call".into(),
            name: ToolName::new("delegate"),
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
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-top".to_owned(),
        },

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("top response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            query_id: "q-leaf".to_owned(),
            agent_id: "test-agent-q-leaf".to_owned(),
            instruction: "LEAF: do one terminal search only".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: Some("leaf-call".into()),
            task_name: Some("leaf".to_owned()),
        },
    )
    .expect("leaf query");

    let leaf_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            matches!(
                h.conversations
                    .get(prompt_cid)
                    .map(|conv| &conv.originator),
                Some(tau_proto::PromptOriginator::Extension { query_id, .. }) if query_id == "q-leaf"
            )
            .then_some(spid.clone())
        })
        .expect("leaf prompt id");
    let prompt = read_prompt_created(&h, &leaf_spid);
    let rendered = prompt
        .context_items
        .iter()
        .filter_map(text_part)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("LEAF: do one terminal search only"),
        "leaf prompt must include its own instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("TOP: delegate exactly two more subtasks"),
        "leaf prompt must not inherit parent recursive instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("ROOT: ask top delegate to delegate again"),
        "leaf prompt must not inherit ancestor task framing; got: {rendered}",
    );

    let tool_uses: Vec<String> = prompt
        .context_items
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    assert!(
        tool_uses.is_empty(),
        "leaf prompt must not inherit unresolved ancestor tool calls; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a delayed response for an older prompt in the same conversation
/// must not be allowed to append fresh tool calls after a newer prompt is
/// already in flight. That creates orphan `function_call` items with no
/// matching output in later full replays, which OpenAI rejects with `No tool
/// output found for function call …`.
#[test]
fn stale_same_conversation_tool_call_response_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    let old_spid: SessionPromptId = "sp-old".into();
    let new_spid: SessionPromptId = "sp-new".into();
    h.prompt_conversations.insert(old_spid.clone(), cid.clone());
    h.prompt_conversations.insert(new_spid.clone(), cid.clone());
    {
        let conv = h.conversations.get_mut(&cid).expect("default conversation");
        conv.in_flight_prompt = Some(new_spid.clone());
        conv.last_prompt_id = Some(new_spid.clone());
    }

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: old_spid.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "stale-call".into(),
            name: ToolName::new("wait"),
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
    .expect("stale response ignored");

    assert!(
        !event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ToolRequest(request) if request.call_id.as_str() == "stale-call"
        )),
        "stale tool call must not be dispatched",
    );
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::SessionPromptTerminated(terminated)
                if terminated.session_prompt_id.as_str() == old_spid.as_str()
                    && terminated.reason == tau_proto::SessionPromptTerminationReason::Stale
        )),
        "stale prompt must publish a terminal lifecycle event",
    );
    assert!(!h.prompt_conversations.contains_key(old_spid.as_str()));
    let conv = h.conversations.get(&cid).expect("default conversation");
    assert_eq!(conv.in_flight_prompt.as_ref(), Some(&new_spid));
    assert!(matches!(conv.turn_state, ConversationTurnState::Idle));

    h.shutdown().expect("shutdown");
}

/// Regression: parallel side conversations must not steal each
/// other's branch cursor. Before the per-event `folded_node_id`
/// sync, `commit_event` synced `c.head` from the global
/// `tree.head()`. A non-folding event on conv-A (e.g. an
/// `ProviderResponseFinished` carrying only tool calls) would overwrite
/// `c.head[conv-A]` with whatever sibling conv-B last folded — so
/// conv-A's next `ToolRequest` would graft onto conv-B's branch and
/// the resulting prompt would walk through unrelated history,
/// producing orphan ToolUse blocks the provider rejects with
/// `No tool output found for function call …`.
fn message_tool_call(id: &str, recipient_id: &str, message: &str) -> AgentToolCall {
    AgentToolCall {
        id: id.into(),
        name: ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("recipient_id".to_owned()),
                CborValue::Text(recipient_id.to_owned()),
            ),
            (
                CborValue::Text("message".to_owned()),
                CborValue::Text(message.to_owned()),
            ),
        ]),
        display: None,
    }
}

fn session_agent_messages(h: &Harness) -> Vec<tau_proto::AgentMessage> {
    h.store
        .session_events("s1")
        .expect("session events")
        .into_iter()
        .filter_map(|entry| match entry.event {
            Event::AgentMessage(message) => Some(message),
            _ => None,
        })
        .collect()
}

#[test]
fn message_tool_to_user_emits_exactly_one_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-user", "user", "hello user"),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    let messages = session_agent_messages(&h);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].sender_id.starts_with("senior-engineer_"));
    assert_eq!(messages[0].recipient_id, "user");
    assert_eq!(messages[0].message, "hello user");

    h.shutdown().expect("shutdown");
}

/// Unknown agent recipients must fail the tool call before publishing any
/// `AgentMessage`, so a typo cannot create forged transcript state.
#[test]
fn message_tool_unknown_recipient_errors_without_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-bad", "missing_agent", "hello"),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    assert!(session_agent_messages(&h).is_empty());
    let errors: Vec<_> = h
        .store
        .session_events("s1")
        .expect("session events")
        .into_iter()
        .filter_map(|entry| match entry.event {
            Event::ToolError(error) if error.call_id.as_str() == "msg-bad" => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("unknown message recipient"));
    assert!(errors[0].message.contains("unknown"));

    h.shutdown().expect("shutdown");
}

/// A completed agent used to be collapsed with a typo as an unknown recipient.
/// Keep the error distinct so callers can decide whether to retry or fix the
/// id.
#[test]
fn message_tool_stopped_recipient_errors_without_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let stopped_cid = ConversationId::new("stopped-recipient");
    h.conversations.insert(
        stopped_cid.clone(),
        Conversation::new(
            stopped_cid.clone(),
            "s1".into(),
            tau_proto::PromptOriginator::User,
            None,
            None,
        ),
    );
    let recipient_id = h
        .ensure_agent_id_for_conversation(&stopped_cid)
        .expect("agent id");
    h.remove_conversation(&stopped_cid);

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-stopped", &recipient_id, "hello"),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    assert!(session_agent_messages(&h).is_empty());
    let errors: Vec<_> = h
        .store
        .session_events("s1")
        .expect("session events")
        .into_iter()
        .filter_map(|entry| match entry.event {
            Event::ToolError(error) if error.call_id.as_str() == "msg-stopped" => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("stopped message recipient"));
    assert!(errors[0].message.contains("stopped"));

    h.shutdown().expect("shutdown");
}

/// Agent-directed messages are displayed in the UI like every `AgentMessage`,
/// and the recipient agent receives an internal queued prompt with stable
/// markup.
#[test]
fn message_tool_to_agent_queues_internal_prompt_markup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    let recipient_id = h.ensure_agent_id_for_conversation(&cid).expect("agent id");
    h.conversations
        .get_mut(&cid)
        .expect("conversation")
        .turn_state = ConversationTurnState::AgentThinking {
        session_prompt_id: "sp-message-target".into(),
    };

    h.handle_message_tool_call(
        &cid,
        &message_tool_call(
            "msg-agent",
            &recipient_id,
            "secret <message>&</message> payload >",
        ),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    let conv = h.conversations.get(&cid).expect("conversation");
    let queued = conv.pending_prompts.back().expect("queued prompt");
    assert_eq!(
        queued.message_class,
        tau_proto::PromptMessageClass::Internal
    );
    assert!(queued.text.contains(&format!(
        "[tau-internal]: You have received a message from {recipient_id}"
    )));
    assert!(queued.text.contains(
        "<message>\nsecret &lt;message&gt;&amp;&lt;/message&gt; payload &gt;\n</message>"
    ));

    h.shutdown().expect("shutdown");
}

/// Agent ids are minted once per conversation, are role-prefixed, and are
/// removed from the reverse lookup when the conversation is torn down.
#[test]
fn agent_id_generation_is_stable_and_cleaned_up() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();

    let first = h.ensure_agent_id_for_conversation(&cid).expect("agent id");
    let second = h.ensure_agent_id_for_conversation(&cid).expect("agent id");
    assert_eq!(first, second);
    assert!(first.starts_with("senior-engineer_"));
    assert_eq!(first.len(), "senior-engineer_".len() + 8);
    assert_eq!(h.agent_conversations.get(&first), Some(&cid));

    h.remove_conversation(&cid);
    assert!(!h.agent_conversations.contains_key(&first));

    h.shutdown().expect("shutdown");
}

/// External clients and extensions must not forge `AgentMessage` events; only
/// the harness-owned message tool may publish them.
#[test]
fn inbound_agent_message_events_are_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let forged = Event::AgentMessage(tau_proto::AgentMessage {
        session_id: "s1".into(),
        sender_id: "attacker".to_owned(),
        recipient_id: "user".to_owned(),
        message: "forged".to_owned(),
    });
    h.handle_client_event_inner("ui", forged.clone())
        .expect("client event");
    h.handle_extension_event_inner("extension", forged.clone())
        .expect("extension event");
    h.handle_extension_message(
        "extension",
        Message::Emit(tau_proto::Emit {
            event: Box::new(forged),
            transient: false,
        }),
    )
    .expect("extension emit");

    assert!(session_agent_messages(&h).is_empty());

    h.shutdown().expect("shutdown");
}
