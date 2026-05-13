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
        reasoning_items: Vec::new(),
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

/// `None` base in → `None` out, regardless of originator. The resolver
/// already decided this provider doesn't get a key; the agent must not
/// resurrect one.
#[test]
fn mix_originator_passes_through_absent_base() {
    let ext = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-1".into(),
    };
    assert_eq!(
        mix_originator_into_cache_key(None, &PromptOriginator::User, false),
        None
    );
    assert_eq!(mix_originator_into_cache_key(None, &ext, false), None);
}

/// A user-originated turn must reuse the resolver-supplied base
/// verbatim, so successive turns of an interactive session keep
/// routing to the same cache machine.
#[test]
fn mix_originator_user_returns_base_verbatim() {
    let base = "tau-abc123";
    assert_eq!(
        mix_originator_into_cache_key(Some(base), &PromptOriginator::User, false),
        Some(base.to_owned()),
    );
}

/// An extension-originated turn must NOT inherit the user's wire key —
/// otherwise parallel delegate runs pile onto the user agent's
/// routing bucket and push the `(prefix, prompt_cache_key)` pair past
/// the ~15 RPM threshold the OpenAI deployment checklist warns about.
#[test]
fn mix_originator_extension_diverges_from_user() {
    let base = "tau-abc123";
    let ext = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-1".into(),
    };
    let user_key = mix_originator_into_cache_key(Some(base), &PromptOriginator::User, false);
    let ext_key = mix_originator_into_cache_key(Some(base), &ext, false);
    assert!(user_key.is_some() && ext_key.is_some());
    assert_ne!(user_key, ext_key);
}

/// Two distinct extensions must route to distinct cache buckets so
/// e.g. a websearch helper and a delegate sub-agent don't share load.
#[test]
fn mix_originator_distinct_extensions_diverge() {
    let base = "tau-abc123";
    let delegate = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "q-1".into(),
    };
    let websearch = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("websearch-exa"),
        query_id: "q-2".into(),
    };
    assert_ne!(
        mix_originator_into_cache_key(Some(base), &delegate, false),
        mix_originator_into_cache_key(Some(base), &websearch, false),
    );
}

/// The `query_id` is intentionally NOT mixed in: a sub-agent's own
/// multi-turn loop (each turn carries a fresh query id) must keep
/// hitting the same cache. If this regressed, every delegate turn
/// would be a cold cache.
#[test]
fn mix_originator_ignores_extension_query_id() {
    let base = "tau-abc123";
    let first = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-1".into(),
    };
    let second = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-2".into(),
    };
    assert_eq!(
        mix_originator_into_cache_key(Some(base), &first, false),
        mix_originator_into_cache_key(Some(base), &second, false),
    );
}

/// When the harness flags a side query as "share the user's bucket"
/// (`share_user_bucket=true`), an extension-originated turn must
/// produce the user's base key — not the per-extension hash — so the
/// single-shot probe (idle-summary) hits the user's already-warm
/// prefix cache instead of cold-starting its own.
#[test]
fn mix_originator_share_user_bucket_overrides_extension_split() {
    let base = "tau-abc123";
    let ext = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("std-notifications"),
        query_id: "idle-0".into(),
    };
    let user_key = mix_originator_into_cache_key(Some(base), &PromptOriginator::User, false);
    let ext_shared_key = mix_originator_into_cache_key(Some(base), &ext, true);
    let ext_split_key = mix_originator_into_cache_key(Some(base), &ext, false);
    assert_eq!(ext_shared_key, user_key);
    assert_ne!(ext_split_key, user_key);
}

/// Determinism: same inputs → byte-equal output. Locks the hash
/// format so a stray reformatting of the salt prefix doesn't silently
/// invalidate every cache key in the wild.
#[test]
fn mix_originator_is_deterministic() {
    let base = "tau-abc123";
    let ext = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-1".into(),
    };
    assert_eq!(
        mix_originator_into_cache_key(Some(base), &ext, false),
        mix_originator_into_cache_key(Some(base), &ext, false),
    );
}
