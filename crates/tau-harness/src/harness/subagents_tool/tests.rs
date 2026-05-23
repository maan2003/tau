use super::*;

fn conv(id: &str) -> ConversationId {
    ConversationId::new(id)
}

fn wait_args_empty() -> CborValue {
    CborValue::Map(Vec::new())
}

fn wait_args_exact(call_id: &str) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Text(call_id.to_owned()),
    )])
}

fn wait_tool_name() -> ToolName {
    ToolName::new(WAIT_TOOL_NAME)
}

fn slow_tool_name() -> ToolName {
    ToolName::new("slow")
}

fn background_placeholder(call_id: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        result: CborValue::Text("still running".to_owned()),
        kind: ToolResultKind::BackgroundPlaceholder,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn background_result(call_id: &str, text: &str) -> ToolBackgroundResult {
    ToolBackgroundResult {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn background_error(
    call_id: &str,
    message: &str,
    details: Option<CborValue>,
) -> ToolBackgroundError {
    ToolBackgroundError {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        message: message.to_owned(),
        details,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn start_wait_any(tracker: &mut WaitTracker, owner: &ConversationId, call_id: &str) -> WaitStart {
    tracker.handle_wait_invoke(owner, call_id.into(), wait_tool_name(), &wait_args_empty())
}

fn start_wait_exact(
    tracker: &mut WaitTracker,
    owner: &ConversationId,
    wait_call_id: &str,
    target_call_id: &str,
) -> WaitStart {
    tracker.handle_wait_invoke(
        owner,
        wait_call_id.into(),
        wait_tool_name(),
        &wait_args_exact(target_call_id),
    )
}

fn start_reply(start: WaitStart) -> WaitReply {
    start.reply.expect("wait reply")
}

fn reply_result(reply: WaitReply) -> CborValue {
    match reply.kind {
        WaitReplyKind::Result { result, .. } => result,
        other => panic!("expected result reply, got {other:?}"),
    }
}

fn reply_error(reply: WaitReply) -> (String, Option<CborValue>) {
    match reply.kind {
        WaitReplyKind::Error {
            message, details, ..
        } => (message, details),
        other => panic!("expected error reply, got {other:?}"),
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

#[test]
fn message_tool_schema_requires_recipient_and_message() {
    let spec = message_tool_spec();
    let parameters = spec.parameters.expect("parameters");
    assert_eq!(
        parameters["required"],
        serde_json::json!(["recipient_id", "message"])
    );
}

#[test]
fn message_args_require_non_empty_recipient_and_message() {
    let ok = CborValue::Map(vec![
        (
            CborValue::Text("recipient_id".to_owned()),
            CborValue::Text("user".to_owned()),
        ),
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
    ]);
    let parsed = parse_message_args(&ok).expect("valid message args");
    assert_eq!(parsed.recipient_id, "user");
    assert_eq!(parsed.message, "hello");

    let missing = CborValue::Map(vec![(
        CborValue::Text("recipient_id".to_owned()),
        CborValue::Text("user".to_owned()),
    )]);
    assert_eq!(
        parse_message_args(&missing),
        Err("`message` is required".to_owned())
    );

    let empty = CborValue::Map(vec![
        (
            CborValue::Text("recipient_id".to_owned()),
            CborValue::Text(" ".to_owned()),
        ),
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
    ]);
    assert_eq!(
        parse_message_args(&empty),
        Err("`recipient_id` must not be empty".to_owned())
    );
}

/// The no-arg form is intentional, so the agent-visible schema must not
/// force `tool_call_id` even though the exact-id form remains available.
#[test]
fn wait_tool_schema_does_not_require_tool_call_id() {
    let spec = wait_tool_spec();
    let parameters = spec.parameters.expect("parameters");
    let required = parameters
        .get("required")
        .and_then(serde_json::Value::as_array);

    assert!(required.is_none_or(|items| {
        items
            .iter()
            .all(|item| item.as_str() != Some("tool_call_id"))
    }));
}

/// `wait({})` is now the shorthand for waiting on any background
/// completion scoped to the current conversation.
#[test]
fn wait_args_omitted_tool_call_id_parse_as_any_background() {
    assert_eq!(
        parse_wait_args(&wait_args_empty()),
        Ok(WaitTarget::AnyBackground)
    );
    let unrelated = CborValue::Map(vec![(
        CborValue::Text("unused".to_owned()),
        CborValue::Text("ignored".to_owned()),
    )]);
    assert_eq!(parse_wait_args(&unrelated), Ok(WaitTarget::AnyBackground));
}

/// Invalid explicit ids still fail early so a typo does not silently turn
/// into a broad no-arg wait.
#[test]
fn wait_args_reject_non_string_and_empty_tool_call_id() {
    let non_string = CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Bool(true),
    )]);
    assert_eq!(
        parse_wait_args(&non_string),
        Err("`tool_call_id` must be a string".to_owned())
    );
    assert_eq!(
        parse_wait_args(&wait_args_exact("   ")),
        Err("`tool_call_id` must not be empty".to_owned())
    );
}

/// Completed any-waits must use deterministic finish order, not HashMap
/// iteration order. The call that finishes first is consumed first even if
/// its id sorts after a later completion.
#[test]
fn no_arg_wait_consumes_oldest_completed_background_result_for_owner() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("bg-b", "first finished"), owner.clone())
            .is_empty()
    );
    assert!(
        tracker
            .record_background_result(background_result("bg-a", "second finished"), owner.clone())
            .is_empty()
    );

    let first = start_wait_any(&mut tracker, &owner, "wait-first");
    assert_eq!(
        first.suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-b")
    );
    let first_result = reply_result(start_reply(first));
    assert_eq!(
        cbor_map_text(&first_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-b")
    );
    assert_eq!(
        cbor_map_text(&first_result, "output"),
        Some("first finished")
    );

    let second = start_wait_any(&mut tracker, &owner, "wait-second");
    assert_eq!(
        second.suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-a")
    );
    let second_result = reply_result(start_reply(second));
    assert_eq!(
        cbor_map_text(&second_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-a")
    );
    assert_eq!(
        cbor_map_text(&second_result, "output"),
        Some("second finished")
    );
}

/// If a same-conversation background call is still running, `wait({})`
/// must block and resolve when the first matching completion arrives.
#[test]
fn no_arg_wait_blocks_on_running_background_call_and_resolves() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-run"), owner.clone())
            .is_empty()
    );

    let start = start_wait_any(&mut tracker, &owner, "wait-any");
    assert!(start.reply.is_none());
    assert!(start.suppress_call_id.is_none());

    let replies = tracker.record_background_result(background_result("bg-run", "done"), owner);
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-run")
    );
    let result = reply_result(replies.into_iter().next().expect("reply"));
    assert_eq!(
        cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-run")
    );
    assert_eq!(cbor_map_text(&result, "output"), Some("done"));
}

/// Background errors are completions too. A no-arg wait must return the
/// error and include the original background id in provider-visible
/// details.
#[test]
fn no_arg_wait_returns_background_error_with_original_id_details() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    let details = CborValue::Map(vec![(
        CborValue::Text("hint".to_owned()),
        CborValue::Text("bad input".to_owned()),
    )]);
    assert!(
        tracker
            .record_background_error(
                background_error("bg-fail", "boom", Some(details)),
                owner.clone()
            )
            .is_empty()
    );

    let reply = start_reply(start_wait_any(&mut tracker, &owner, "wait-error"));
    let (message, details) = reply_error(reply);
    assert_eq!(message, "boom");
    let details = details.expect("details");
    assert_eq!(
        cbor_map_text(&details, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-fail")
    );
    assert_eq!(cbor_map_text(&details, "hint"), Some("bad input"));
}

/// When an exact waiter and an any-waiter can both see the same completion,
/// the exact waiter gets the result and the any-waiter does not consume a
/// duplicate copy.
#[test]
fn explicit_waiter_wins_over_any_waiter_for_same_completion() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-run"), owner.clone())
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-any")
            .reply
            .is_none()
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait-exact", "bg-run")
            .reply
            .is_none()
    );

    let replies = tracker.record_background_result(background_result("bg-run", "done"), owner);
    assert!(replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-exact"
            && matches!(reply.kind, WaitReplyKind::Result { .. })
    }));
    assert!(replies.iter().all(|reply| {
        reply.wait_call_id.as_str() != "wait-any"
            || matches!(reply.kind, WaitReplyKind::Error { .. })
    }));
}

/// Parallel duplicate no-arg waits in one conversation would be ambiguous:
/// only one waiter may consume the next completion.
#[test]
fn duplicate_no_arg_waits_in_same_conversation_error() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-run"), owner.clone())
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-one")
            .reply
            .is_none()
    );

    let (message, details) = reply_error(start_reply(start_wait_any(
        &mut tracker,
        &owner,
        "wait-two",
    )));
    assert!(message.contains("existing wait for a background tool call"));
    assert!(details.is_none());
}

/// The no-arg form is scoped to its caller's conversation. A completion in
/// a different conversation must not be stolen by this wait.
#[test]
fn no_arg_wait_ignores_background_completions_from_other_conversations() {
    let main = conv("main");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("side-bg", "side done"), side.clone())
            .is_empty()
    );

    let (message, _) = reply_error(start_reply(start_wait_any(
        &mut tracker,
        &main,
        "wait-main",
    )));
    assert!(message.contains("no background tool calls"));

    let side_result = reply_result(start_reply(start_wait_any(
        &mut tracker,
        &side,
        "wait-side",
    )));
    assert_eq!(
        cbor_map_text(&side_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("side-bg")
    );
}

/// Once a no-arg wait consumes a completion, a later exact wait for that
/// original id must report that the result was already handled.
#[test]
fn exact_wait_after_no_arg_consumes_reports_already_consumed() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("bg-once", "done"), owner.clone())
            .is_empty()
    );
    let _ = start_reply(start_wait_any(&mut tracker, &owner, "wait-any"));

    let (message, _) = reply_error(start_reply(start_wait_exact(
        &mut tracker,
        &owner,
        "wait-exact",
        "bg-once",
    )));
    assert!(message.contains("already consumed"));
}

/// Side-conversation teardown transfers background ownership to the parent;
/// the wait tracker must follow that transfer so the parent can consume a
/// completion with `wait({})` after the side conversation disappears.
#[test]
fn transferred_background_owner_can_be_consumed_by_parent_no_arg_wait() {
    let parent = conv("parent");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("bg-side", "done"), side.clone())
            .is_empty()
    );
    tracker.transfer_call_owner(&"bg-side".into(), &side, &parent);

    let result = reply_result(start_reply(start_wait_any(
        &mut tracker,
        &parent,
        "wait-parent",
    )));
    assert_eq!(
        cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-side")
    );
    assert_eq!(cbor_map_text(&result, "output"), Some("done"));
}
