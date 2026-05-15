use tau_config::settings::PromptCacheRetention;
use tau_proto::{ContentBlock, ConversationMessage, ConversationRole};

use super::*;
use crate::common::{LlmError, PreviousResponse};

#[test]
fn build_request_includes_prompt_cache_fields_when_configured() {
    let config = ResponsesConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: true,
        prompt_cache_retention: Some(PromptCacheRetention::InMemory),
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let prompt_cache_key = body["prompt_cache_key"].as_str().expect("prompt_cache_key");

    assert!(prompt_cache_key.starts_with("tau-"));
    assert_eq!(body["prompt_cache_retention"], "in_memory");
}

#[test]
fn build_request_includes_service_tier_when_configured() {
    let config = ResponsesConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams {
            service_tier: Some(tau_proto::ServiceTier::Fast),
            ..Default::default()
        },
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(body["service_tier"], "priority");
}

#[test]
fn build_request_omits_prompt_cache_fields_without_seed_or_retention() {
    let config = ResponsesConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let object = body.as_object().expect("request object");

    assert!(!object.contains_key("prompt_cache_key"));
    assert!(!object.contains_key("prompt_cache_retention"));
}

/// First turn (no chain established): the request must contain the
/// full transcript, `store: false`, and no `previous_response_id`.
/// This is the baseline that future stateful-chain optimizations are
/// compared against; if it ever flips, every turn would start
/// charging for stored responses by accident.
#[test]
fn build_request_first_turn_replays_full_history_without_chain() {
    let config = chain_test_config();
    let messages = vec![user_text("hello"), assistant_text("hi there")];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    // Two messages → two `input` items (one user text, one assistant message).
    assert_eq!(
        input.len(),
        2,
        "full history must be replayed when chain is absent"
    );
}

#[test]
fn build_compact_request_omits_store_field() {
    let config = chain_test_config();
    let messages = vec![user_text("hello")];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_compact_request(&config, &request)).expect("serialize");
    let object = body.as_object().expect("request body is an object");

    assert!(
        !object.contains_key("store"),
        "the compact endpoint rejects store entirely"
    );
}

/// Stateful-chain turn: when the harness supplies a
/// `previous_response`, the request body slices off the prefix
/// already covered by that response and pins the prior `response.id`.
/// `store` stays `false` — the Codex endpoint *rejects* `store: true`
/// (`HTTP 400 {"detail":"Store must be set to false"}`) even when
/// chaining, in contrast with the public Responses API. Tau today
/// only routes Responses through Codex, so this asserts the Codex
/// shape; a future public-API path would need a separate test.
#[test]
fn build_request_chain_turn_sends_delta_and_previous_response_id() {
    let config = chain_test_config();
    // Full transcript: 1 user, 1 assistant, 1 user tool-result.
    // Chain anchor was captured after the assistant turn
    // (message_index = 2), so only the trailing tool-result should
    // make it into the request.
    let messages = vec![
        user_text("first turn"),
        assistant_text("first response"),
        user_text("second turn"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: Some(PreviousResponse {
            id: "resp_abc",
            message_index: 2,
            transport: Some(tau_proto::AgentBackendTransport::HttpSse),
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(
        body["store"], false,
        "Codex rejects store=true even when chaining"
    );
    assert_eq!(body["previous_response_id"], "resp_abc");
    let input = body["input"].as_array().expect("input array");
    assert_eq!(
        input.len(),
        1,
        "only messages after the anchor should be sent"
    );
    assert_eq!(input[0]["content"][0]["text"], "second turn");
}

/// Defensive: a stale `message_index` (somehow larger than the
/// assembled transcript) must NOT panic and must NOT chain — fall
/// back to a full-replay first-turn-style request so the conversation
/// keeps working instead of crashing the agent.
#[test]
fn build_request_chain_with_oob_index_falls_back_to_full_replay() {
    let config = chain_test_config();
    let messages = vec![user_text("only")];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: Some(PreviousResponse {
            id: "resp_abc",
            message_index: 99,
            transport: Some(tau_proto::AgentBackendTransport::HttpSse),
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1);
}

/// A 4xx body that mentions `previous_response` is the signal that
/// the upstream's stored state for our chain has been evicted. We
/// detect this so the in-agent fallback can re-send with a full
/// transcript without bothering the user.
#[test]
fn stale_chain_error_detection() {
    let stale = LlmError::HttpStatus(
        400,
        r#"{"error":{"message":"previous_response_id 'resp_x' not found"}}"#.into(),
    );
    assert!(is_stale_chain_error(&stale));

    let unrelated = LlmError::HttpStatus(400, r#"{"error":{"message":"bad request"}}"#.into());
    assert!(!is_stale_chain_error(&unrelated));

    // 500s flow through the harness's transient-retry path, not the
    // chain-fallback path — even if the body happens to mention the
    // chain.
    let server = LlmError::HttpStatus(503, "previous_response upstream blip".into());
    assert!(!is_stale_chain_error(&server));
}

/// Regression: `prompt_cache_key` must still ride along on chained
/// (`previous_response_id`) turns. Without it the Codex backend would
/// route the chain continuation to a different machine on each turn
/// and squander the warm cache the chain is supposed to preserve.
#[test]
fn build_request_chain_turn_still_emits_prompt_cache_key() {
    let config = ResponsesConfig {
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let messages = vec![
        user_text("first turn"),
        assistant_text("first response"),
        user_text("second turn"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: Some(PreviousResponse {
            id: "resp_abc",
            message_index: 2,
            transport: Some(tau_proto::AgentBackendTransport::HttpSse),
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    assert_eq!(body["previous_response_id"], "resp_abc");
    assert!(body["prompt_cache_key"].is_string());
}

/// The Responses backend must split the wire `prompt_cache_key` for
/// extension-originated turns just like the Chat Completions backend
/// does. Both paths share the same `mix_originator_into_cache_key`
/// helper, but this test pins the wiring at the request-build layer
/// so a future refactor can't silently regress it on one path.
#[test]
fn build_request_prompt_cache_key_differs_for_extension_originator() {
    let config = ResponsesConfig {
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-1".into(),
    };
    let user_request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };
    let ext_request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &ext,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let user_body = serde_json::to_value(build_request(&config, &user_request)).expect("serialize");
    let ext_body = serde_json::to_value(build_request(&config, &ext_request)).expect("serialize");

    assert!(user_body["prompt_cache_key"].is_string());
    assert!(ext_body["prompt_cache_key"].is_string());
    assert_ne!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
}

/// Single-shot side queries (idle-summary): `share_user_cache_key`
/// makes the extension turn ride on the user's base
/// `prompt_cache_key` so it hits the user's already-warm prefix
/// cache. Regression for the "the keepalive's there but the
/// idle-summary still costs ~70k uncached tokens" issue traced in
/// session `tau-agent-qv103q` analysis.
#[test]
fn build_request_share_user_cache_key_pins_extension_to_user_bucket() {
    let config = ResponsesConfig {
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("std-notifications"),
        query_id: "idle-0".into(),
    };
    let shared_request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        previous_response: None,
        originator: &ext,
        share_user_cache_key: true,
        session_id: &tau_proto::SessionId::new("test-session"),
    };
    let body = serde_json::to_value(build_request(&config, &shared_request)).expect("serialize");
    assert!(body["prompt_cache_key"].is_string());
}

#[test]
fn build_request_cache_shared_extension_matches_user_wire_body() {
    let config = ResponsesConfig {
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("std-notifications"),
        query_id: "idle-0".into(),
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: Some("run shell commands".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let messages = [user_text("summarize")];
    let previous_response = Some(PreviousResponse {
        id: "resp_parent",
        message_index: 0,
        transport: Some(tau_proto::AgentBackendTransport::HttpSse),
    });
    let user_request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        previous_response,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::new("test-session"),
    };
    let shared_ext_request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        previous_response,
        originator: &ext,
        share_user_cache_key: true,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let user_body = serde_json::to_value(build_request(&config, &user_request)).expect("serialize");
    let ext_body =
        serde_json::to_value(build_request(&config, &shared_ext_request)).expect("serialize");

    assert_eq!(ext_body, user_body);
    assert_eq!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
    assert_eq!(ext_body["tool_choice"], "auto");
    assert_eq!(ext_body["previous_response_id"], "resp_parent");
}

/// `ToolChoice::None` emits `tool_choice: "none"` on the Responses
/// body while leaving the `tools` array fully declared. That is valid
/// for callers that intentionally want a different wire request, but
/// the harness must not use it for cache-sharing side queries because
/// the field participates in provider request equivalence. Verified
/// here on a request that carries real tool definitions.
#[test]
fn build_request_emits_tool_choice_none_while_keeping_tools_declared() {
    let config = chain_test_config();
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        compacted_input_items: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::None,
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(body["tool_choice"], "none");
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        1,
        "tools must stay declared so the cache prefix matches"
    );
}

fn chain_test_config() -> ResponsesConfig {
    ResponsesConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_encrypted_reasoning: false,
    }
}

fn phase_test_config() -> ResponsesConfig {
    ResponsesConfig {
        supports_phase: true,
        ..chain_test_config()
    }
}

fn encrypted_reasoning_test_config() -> ResponsesConfig {
    ResponsesConfig {
        supports_encrypted_reasoning: true,
        ..chain_test_config()
    }
}

fn user_text(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: ConversationRole::User,
        content: vec![ContentBlock::Text { text: text.into() }],
        phase: None,
    }
}

fn assistant_text(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: ConversationRole::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        phase: None,
    }
}

fn assistant_text_with_phase(text: &str, phase: tau_proto::MessagePhase) -> ConversationMessage {
    ConversationMessage {
        role: ConversationRole::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        phase: Some(phase),
    }
}

/// When `supports_phase` is on, every assistant `message` item must
/// carry a `phase` field. A stored `Commentary` value rides straight
/// through; absence of a stored value falls back to `final_answer`
/// per the OpenAI deployment-checklist guidance for legacy history.
#[test]
fn build_request_stamps_phase_on_assistant_messages_when_supported() {
    let config = phase_test_config();
    let messages = vec![
        user_text("hello"),
        assistant_text_with_phase("draft", tau_proto::MessagePhase::Commentary),
        user_text("more"),
        assistant_text("legacy turn without phase"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");

    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(assistant_items.len(), 2, "two assistant messages expected");
    assert_eq!(assistant_items[0]["phase"], "commentary");
    assert_eq!(
        assistant_items[1]["phase"], "final_answer",
        "legacy assistant message must default to final_answer per OpenAI guidance"
    );
}

/// `supports_phase: false` keeps the field off the wire entirely,
/// even when the stored message carries one. This is the safety
/// gate that lets older Codex models (which would reject unknown
/// fields) keep working as the harness sends them history that may
/// have been captured against a newer model.
#[test]
fn build_request_omits_phase_when_unsupported() {
    let config = chain_test_config(); // supports_phase: false
    let messages = vec![assistant_text_with_phase(
        "draft",
        tau_proto::MessagePhase::Commentary,
    )];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_item = input
        .iter()
        .find(|i| i["role"].as_str() == Some("assistant"))
        .expect("assistant message");
    assert!(
        assistant_item
            .as_object()
            .expect("assistant item is an object")
            .get("phase")
            .is_none(),
        "phase must not be sent when the backend doesn't advertise support"
    );
}

/// Tool-call boundaries flush a pending text block into its own
/// assistant `message` item. That intermediate flush must carry
/// `phase` too — otherwise a mixed text+tool_use turn would
/// half-pass-through with a phase on the trailing flush only.
#[test]
fn build_request_stamps_phase_on_pre_tool_call_text_flush() {
    let config = phase_test_config();
    let messages = vec![ConversationMessage {
        role: ConversationRole::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "thinking out loud".into(),
            },
            ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "shell".into(),
                tool_type: tau_proto::ToolType::Function,
                input: tau_proto::CborValue::Null,
            },
            ContentBlock::Text {
                text: "trailing".into(),
            },
        ],
        phase: Some(tau_proto::MessagePhase::Commentary),
    }];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(
        assistant_items.len(),
        2,
        "pre-tool-call text and trailing text each become their own assistant message"
    );
    for (i, item) in assistant_items.iter().enumerate() {
        assert_eq!(
            item["phase"], "commentary",
            "assistant message #{i} must carry the captured phase"
        );
    }
}

/// `parse_phase_from_item` is the bridge from the Responses-API
/// `output_item.*` shape into our typed `MessagePhase`. The two
/// known wire strings round-trip; unknown values (forward
/// compatibility) come back as `None` rather than panicking.
#[test]
fn parse_phase_from_item_recognizes_wire_strings() {
    let commentary = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&commentary),
        Some(tau_proto::MessagePhase::Commentary)
    );

    let final_ans = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "final_answer",
    });
    assert_eq!(
        parse_phase_from_item(&final_ans),
        Some(tau_proto::MessagePhase::FinalAnswer)
    );

    let unknown_future = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "rumination",
    });
    assert_eq!(parse_phase_from_item(&unknown_future), None);

    let no_phase = serde_json::json!({
        "type": "message",
        "role": "assistant",
    });
    assert_eq!(parse_phase_from_item(&no_phase), None);

    let function_call = serde_json::json!({
        "type": "function_call",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&function_call),
        None,
        "non-message items must not have their `phase` field harvested"
    );
}

// -----------------------------------------------------------------------
// Encrypted reasoning replay
// -----------------------------------------------------------------------

/// `supports_encrypted_reasoning: true` must put
/// `include: ["reasoning.encrypted_content"]` on the request body.
/// Without this opt-in the model returns `reasoning` items but with
/// no replayable content — we'd persist empty husks and lose the
/// continuity the whole feature buys.
#[test]
fn build_request_emits_include_when_encrypted_reasoning_supported() {
    let config = encrypted_reasoning_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let include = body["include"].as_array().expect("include array");
    assert_eq!(include.len(), 1);
    assert_eq!(include[0], "reasoning.encrypted_content");
}

/// `supports_encrypted_reasoning: false` keeps the `include` field
/// out of the request entirely — older endpoints (and the public
/// Responses API) reject unknown opt-ins, so we don't even want an
/// empty `include: []` on the wire.
#[test]
fn build_request_omits_include_when_encrypted_reasoning_unsupported() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("include")
            .is_none(),
        "include must be omitted when the provider doesn't advertise support"
    );
}

/// A `ContentBlock::Reasoning` on an assistant message must be
/// emitted as its own top-level `input[]` item — same structural
/// slot as `message` and `function_call`, NEVER nested inside the
/// assistant message. Locks in the Pi-compatible replay shape; if
/// this regresses, the model loses reasoning continuity across a
/// broken chain.
#[test]
fn build_request_replays_reasoning_item_as_top_level_input() {
    let config = encrypted_reasoning_test_config();
    let reasoning_blob = serde_json::json!({
        "type": "reasoning",
        "id": "rs_abc123",
        "summary": [],
        "encrypted_content": "OPAQUE-BLOB"
    })
    .to_string();
    let messages = vec![ConversationMessage {
        role: ConversationRole::Assistant,
        content: vec![
            ContentBlock::Reasoning {
                item: reasoning_blob,
            },
            ContentBlock::Text {
                text: "here's the answer".into(),
            },
        ],
        phase: None,
    }];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let reasoning_idx = input
        .iter()
        .position(|item| item["type"].as_str() == Some("reasoning"))
        .expect("reasoning input item");
    let message_idx = input
        .iter()
        .position(|item| item["role"].as_str() == Some("assistant"))
        .expect("assistant message item");
    assert!(
        reasoning_idx < message_idx,
        "reasoning must precede the assistant message it relates to (Pi-compatible order); \
         reasoning_idx={reasoning_idx}, message_idx={message_idx}"
    );
    let reasoning = &input[reasoning_idx];
    assert_eq!(reasoning["id"], "rs_abc123");
    assert_eq!(
        reasoning["encrypted_content"], "OPAQUE-BLOB",
        "the opaque blob must round-trip verbatim — the harness must not parse fields out"
    );
}

#[test]
fn build_request_emits_custom_tool_definition_and_round_trips_custom_tool_output() {
    let config = chain_test_config();
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("apply_patch"),
        model_visible_name: None,
        description: Some("Apply a patch to files".to_owned()),
        tool_type: tau_proto::ToolType::Custom,
        parameters: None,
        format: Some(tau_proto::ToolFormat::Grammar {
            syntax: tau_proto::ToolGrammarSyntax::Regex,
            definition: "(?s).+".to_owned(),
        }),
    };
    let messages = vec![
        ConversationMessage {
            role: ConversationRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call-patch".into(),
                name: "apply_patch".into(),
                tool_type: tau_proto::ToolType::Custom,
                input: tau_proto::CborValue::Text("*** Begin Patch\n*** End Patch".into()),
            }],
            phase: None,
        },
        ConversationMessage {
            role: ConversationRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-patch".into(),
                content: "ok".into(),
                is_error: false,
            }],
            phase: None,
        },
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["type"], "custom");
    assert_eq!(tools[0]["name"], "apply_patch");
    assert_eq!(tools[0]["format"]["type"], "grammar");
    assert_eq!(tools[0]["format"]["syntax"], "regex");
    assert_eq!(tools[0]["format"]["definition"], "(?s).+");

    let input = body["input"].as_array().expect("input");
    assert_eq!(input[0]["type"], "custom_tool_call");
    assert_eq!(input[0]["call_id"], "call-patch");
    assert_eq!(input[0]["input"], "*** Begin Patch\n*** End Patch");
    assert_eq!(input[1]["type"], "custom_tool_call_output");
    assert_eq!(input[1]["call_id"], "call-patch");
    assert_eq!(input[1]["output"], "ok");
}

#[test]
fn apply_event_accumulates_custom_tool_input_deltas() {
    use crate::common::StreamState;

    let mut state = StreamState::new();
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "type": "custom_tool_call",
            "call_id": "call_patch",
            "name": "apply_patch",
        }
    });
    apply_event(&mut state, &added, &mut |_, _| {}).expect("added");
    let delta = serde_json::json!({
        "type": "response.custom_tool_call_input.delta",
        "output_index": 0,
        "delta": "*** Begin Patch"
    });
    apply_event(&mut state, &delta, &mut |_, _| {}).expect("delta");
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "custom_tool_call",
            "call_id": "call_patch",
            "name": "apply_patch",
            "input": "*** Begin Patch"
        }
    });
    apply_event(&mut state, &done, &mut |_, _| {}).expect("done");

    assert_eq!(state.tool_calls.len(), 1);
    assert_eq!(state.tool_calls[0].tool_type, tau_proto::ToolType::Custom);
    assert_eq!(state.tool_calls[0].id, "call_patch");
    assert_eq!(state.tool_calls[0].name, "apply_patch");
    assert_eq!(state.tool_calls[0].arguments_json, "*** Begin Patch");
}

#[test]
fn build_request_chain_keeps_custom_tool_output_type_from_prior_history() {
    let config = chain_test_config();
    let messages = vec![
        ConversationMessage {
            role: ConversationRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call-custom".into(),
                name: "apply_patch".into(),
                tool_type: tau_proto::ToolType::Custom,
                input: tau_proto::CborValue::Text("patch body".into()),
            }],
            phase: None,
        },
        ConversationMessage {
            role: ConversationRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-custom".into(),
                content: "ok".into(),
                is_error: false,
            }],
            phase: None,
        },
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        previous_response: Some(PreviousResponse {
            id: "resp_prev",
            message_index: 1,
            transport: Some(tau_proto::AgentBackendTransport::HttpSse),
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(
        input.len(),
        1,
        "only the trailing tool result should be replayed"
    );
    assert_eq!(input[0]["type"], "custom_tool_call_output");
    assert_eq!(input[0]["call_id"], "call-custom");
}

/// On the Codex Responses stream, `response.output_item.done` is the
/// canonical place to capture a `reasoning` item: it's the only
/// event that carries the final `encrypted_content`. The `added`
/// counterpart fires before any content streams in, so capturing
/// from `added` would persist empty husks. Pin the boundary here so
/// a future refactor of the SSE/WS parser can't silently swap which
/// event we read.
#[test]
fn apply_event_captures_reasoning_only_on_output_item_done() {
    use crate::common::StreamState;
    let mut state = StreamState::new();
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "type": "reasoning",
            "id": "rs_pending",
            "summary": [],
        }
    });
    apply_event(&mut state, &added, &mut |_, _| {}).expect("added");
    assert!(
        state.reasoning_items.is_empty(),
        "`added` carries no encrypted_content — capturing here would persist an empty husk"
    );
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "reasoning",
            "id": "rs_done",
            "summary": [{"type": "summary_text", "text": "thought"}],
            "encrypted_content": "SEALED",
        }
    });
    apply_event(&mut state, &done, &mut |_, _| {}).expect("done");
    assert_eq!(state.reasoning_items.len(), 1);
    let parsed: serde_json::Value = serde_json::from_str(&state.reasoning_items[0]).expect("json");
    assert_eq!(parsed["id"], "rs_done");
    assert_eq!(parsed["encrypted_content"], "SEALED");
}

// -----------------------------------------------------------------------
// WebSocket envelope wrapping
// -----------------------------------------------------------------------

/// The WS guide requires every client frame to carry `type:
/// "response.create"` at the top level. The HTTP body does not.
/// [`build_ws_envelope`] is the only place we add the tag — pin it
/// here so a future refactor that drops the wrapper struct can't
/// silently regress it.
#[test]
fn ws_envelope_adds_type_and_drops_stream() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let http_body = serde_json::to_value(build_request(&config, &request)).expect("http body");
    let ws_body = serde_json::to_value(build_ws_envelope(&config, &request)).expect("ws envelope");

    assert_eq!(ws_body["type"], "response.create");
    let ws_object = ws_body.as_object().expect("WS envelope object");
    assert!(
        ws_object.get("stream").is_none(),
        "WS frame must omit `stream` — the WS guide says it's not used and the field is transport-implicit"
    );
    // Every other body shape stays identical so the request-build
    // tests already in this file double as WS-envelope coverage.
    assert!(
        ws_object.get("generate").is_none(),
        "normal streaming WS turns must preserve the old wire shape"
    );
    assert_eq!(ws_body["model"], http_body["model"]);
    assert_eq!(ws_body["store"], http_body["store"]);
    assert_eq!(ws_body["input"], http_body["input"]);
}

#[test]
fn ws_prewarm_envelope_sets_generate_false_and_drops_previous_response() {
    let config = chain_test_config();
    let messages = vec![user_text("AGENTS.md context")];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        compacted_input_items: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: Some(PreviousResponse {
            id: "resp_previous",
            message_index: 1,
            transport: Some(tau_proto::AgentBackendTransport::HttpSse),
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_ws_prewarm_envelope(&config, &request))
        .expect("prewarm envelope");

    assert_eq!(body["type"], "response.create");
    assert_eq!(body["generate"], false);
    let object = body.as_object().expect("prewarm envelope object");
    assert!(object.get("stream").is_none());
    assert!(object.get("previous_response_id").is_none());
}

// -----------------------------------------------------------------------
// apply_event — shared event applicator for SSE + WS
// -----------------------------------------------------------------------

/// `response.output_text.delta` accumulates into `state.text` and
/// fires `on_update` once per delta. Mirrors the original SSE-only
/// behavior — keeps the WS path equivalent.
#[test]
fn apply_event_text_delta_accumulates_and_notifies() {
    let mut state = crate::common::StreamState::new();
    let mut updates: Vec<String> = Vec::new();
    let mut on_update = |text: &str, _thinking: Option<&str>| {
        updates.push(text.to_owned());
    };

    for chunk in ["hel", "lo, ", "world"] {
        let ev = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": chunk,
        });
        let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
        assert!(!done, "text delta should not terminate the stream");
    }
    assert_eq!(state.text, "hello, world");
    assert_eq!(updates, vec!["hel", "hello, ", "hello, world"]);
}

#[test]
fn apply_event_completed_terminates_and_captures_response_id() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "resp_xyz",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "input_tokens_details": { "cached_tokens": 5 },
            },
        },
    });
    let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
    assert!(done, "response.completed must terminate the stream");
    assert_eq!(state.response_id.as_deref(), Some("resp_xyz"));
    assert_eq!(state.input_tokens, Some(42));
    assert_eq!(state.output_tokens, Some(7));
    assert_eq!(state.cached_tokens, Some(5));
}

#[test]
fn apply_event_function_call_assembles_tool_call() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_a",
                "name": "shell",
            },
        }),
        &mut on_update,
    )
    .expect("ok");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"cmd\":\"ls\"}",
        }),
        &mut on_update,
    )
    .expect("ok");

    assert_eq!(state.tool_calls.len(), 1);
    assert_eq!(state.tool_calls[0].id, "call_a");
    assert_eq!(state.tool_calls[0].name, "shell");
    assert_eq!(state.tool_calls[0].arguments_json, "{\"cmd\":\"ls\"}");
}

#[test]
fn apply_event_failed_returns_error() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": { "message": "model overloaded" },
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("response failed"));
            assert!(body.contains("model overloaded"));
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// Streaming `error` event in the documented OpenAI Responses shape:
/// `{ type: "error", code: <code>, message: <msg> }` (no nested
/// `error` object). The retry classifier needs the code in the
/// `(type=...)` suffix to distinguish account caps from transport
/// hiccups.
#[test]
fn apply_event_error_top_level_code_is_propagated() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "error",
        "code": "rate_limit_exceeded",
        "message": "Rate limit reached",
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("Rate limit reached"));
            assert!(
                body.contains("(type=rate_limit_exceeded)"),
                "missing (type=...) suffix in {body:?}",
            );
            assert!(
                crate::common::is_account_limit_body(&body),
                "is_account_limit_body must classify this body as a cap"
            );
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// Nested `error.code` shape — some Codex error envelopes wrap the
/// code in an `error` object alongside the message. Must produce the
/// same suffix as the top-level form.
#[test]
fn apply_event_error_nested_code_is_propagated() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "error",
        "error": {
            "code": "usage_limit_reached",
            "message": "The usage limit has been reached",
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("usage limit has been reached"));
            assert!(
                body.contains("(type=usage_limit_reached)"),
                "missing (type=...) suffix in {body:?}",
            );
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// Legacy nested `error.type` shape — kept as a fallback so an older
/// recorded session log replayed through the agent still classifies
/// correctly.
#[test]
fn apply_event_error_nested_type_fallback_is_propagated() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "error",
        "error": {
            "type": "quota_exceeded",
            "message": "quota",
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(
                body.contains("(type=quota_exceeded)"),
                "missing (type=...) suffix in {body:?}",
            );
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// No code/type anywhere: body still produced, just without the
/// `(type=...)` suffix. The outer retry layer keeps retrying (we
/// can't safely classify), but we don't crash or drop the message.
#[test]
fn apply_event_error_without_code_omits_suffix() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "error",
        "message": "something broke",
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("something broke"));
            assert!(!body.contains("(type="), "unexpected suffix in {body:?}");
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}
