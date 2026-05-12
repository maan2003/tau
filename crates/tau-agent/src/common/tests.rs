use super::*;

#[test]
fn into_tool_calls_drops_nameless_accumulator_artifacts() {
    // The streaming paths eagerly extend `tool_calls` from
    // argument-delta events so the index stays addressable. If
    // the matching name-carrying event never arrives (partial
    // item, reasoning noise, stream cancellation), the slot stays
    // nameless. Shipping it downstream would trigger a visible
    // `invalid_tool` rejection in the harness and confuse the
    // model, which never intended a second tool call.
    let state = StreamState {
        text: String::new(),
        tool_calls: vec![
            ToolCallAccumulator {
                id: String::new(),
                name: String::new(),
                arguments_json: String::from("{\"stray\": \"delta\"}"),
            },
            ToolCallAccumulator {
                id: "call_real".into(),
                name: "shell".into(),
                arguments_json: "{\"command\":\"ls\"}".into(),
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        response_id: None,
        phase: None,
    };

    let calls = state.into_tool_calls();
    assert_eq!(calls.len(), 1, "nameless accumulator must be dropped");
    assert_eq!(calls[0].id.as_str(), "call_real");
    assert_eq!(calls[0].name.as_str(), "shell");
}

#[test]
fn usage_limit_429_retries_after_reset_seconds() {
    let error = LlmError::HttpStatus(
        429,
        serde_json::json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "resets_in_seconds": 4371
            }
        })
        .to_string(),
    );

    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(4371))
    );
}

#[test]
fn unknown_429_is_not_retryable() {
    let error = LlmError::HttpStatus(
        429,
        serde_json::json!({
            "error": {
                "type": "rate_limit_exceeded",
                "message": "slow down"
            }
        })
        .to_string(),
    );

    assert_eq!(error.retry_after(), None);
}

#[test]
fn server_error_uses_backoff_retry() {
    let error = LlmError::HttpStatus(503, "overloaded".into());

    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}
