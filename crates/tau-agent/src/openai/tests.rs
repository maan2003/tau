use tau_config::settings::PromptCacheRetention;

use super::*;
use crate::common::PromptPayload;

#[test]
fn build_request_includes_prompt_cache_fields_when_configured() {
    let config = OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test".into(),
        model_id: "gpt-5".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: true,
        prompt_cache_retention: Some(PromptCacheRetention::Extended24h),
        supports_llama_cpp_cache: false,
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

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");
    let prompt_cache_key = body["prompt_cache_key"].as_str().expect("prompt_cache_key");

    assert!(prompt_cache_key.starts_with("tau-"));
    assert_eq!(body["prompt_cache_retention"], "24h");
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[test]
fn build_request_includes_service_tier_when_configured() {
    let config = OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test".into(),
        model_id: "gpt-5".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: false,
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

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");

    assert_eq!(body["service_tier"], "priority");
}

#[test]
fn build_request_omits_prompt_cache_fields_without_seed_or_retention() {
    let config = OpenAiConfig {
        base_url: "https://example.com/v1".into(),
        api_key: "test".into(),
        model_id: "local".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: false,
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

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");
    let object = body.as_object().expect("request object");

    assert!(!object.contains_key("prompt_cache_key"));
    assert!(!object.contains_key("prompt_cache_retention"));
    assert!(!object.contains_key("cache_prompt"));
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[test]
fn build_request_includes_llama_cpp_cache_prompt_when_configured() {
    let config = OpenAiConfig {
        base_url: "http://localhost:8080/v1".into(),
        api_key: "none".into(),
        model_id: "llama-3".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: true,
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

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");

    assert_eq!(body["cache_prompt"], true);
}

#[test]
fn build_request_sets_parallel_tool_calls_when_tools_offered() {
    let config = OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test".into(),
        model_id: "gpt-5".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: false,
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "system",
        messages: &[],
        compacted_input_items: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");

    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["tool_choice"], "auto");
}

#[test]
fn build_request_omits_parallel_tool_calls_without_tools() {
    let config = OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test".into(),
        model_id: "gpt-5".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: false,
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

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");
    let object = body.as_object().expect("request object");

    assert!(!object.contains_key("parallel_tool_calls"));
}

#[test]
fn build_request_uses_model_visible_tool_name_when_present() {
    let config = OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test".into(),
        model_id: "gpt-5".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: false,
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("gpt_shell"),
        model_visible_name: Some(tau_proto::ToolName::new("shell")),
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "system",
        messages: &[],
        compacted_input_items: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        share_user_cache_key: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");

    assert_eq!(body["tools"][0]["function"]["name"], "shell");
}

/// `ToolChoice::None` must serialize as `tool_choice: "none"` while
/// the `tools` array stays declared. This is valid when a caller
/// intentionally wants a different wire request. Cache-sharing side
/// queries must not use this lever because the serialized
/// `tool_choice` participates in provider request equivalence.
#[test]
fn build_request_emits_tool_choice_none_while_keeping_tools_declared() {
    let config = OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test".into(),
        model_id: "gpt-5".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: false,
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "system",
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

    let body = serde_json::to_value(build_request(&config, &request, true)).expect("serialize");

    assert_eq!(body["tool_choice"], "none");
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        1,
        "tools must stay declared so the cache prefix matches"
    );
}

/// Extension-originated turns must NOT share the wire
/// `prompt_cache_key` with the user's interactive turns — the OpenAI
/// deployment checklist warns that >15 RPM per
/// `(prefix, prompt_cache_key)` overflows to additional machines and
/// degrades cache hit rate, and parallel delegations easily push a
/// shared key past that threshold.
#[test]
fn build_request_prompt_cache_key_differs_for_extension_originator() {
    let config = OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test".into(),
        model_id: "gpt-5".into(),
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_prompt_cache_key: true,
        prompt_cache_retention: None,
        supports_llama_cpp_cache: false,
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-1".into(),
    };
    let user_request = PromptPayload {
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
    let ext_request = PromptPayload {
        system_prompt: "system",
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

    let user_body =
        serde_json::to_value(build_request(&config, &user_request, true)).expect("serialize");
    let ext_body =
        serde_json::to_value(build_request(&config, &ext_request, true)).expect("serialize");

    assert!(user_body["prompt_cache_key"].is_string());
    assert!(ext_body["prompt_cache_key"].is_string());
    assert_ne!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
}

#[test]
fn stream_choice_accumulates_reasoning_content_as_thinking() {
    let mut state = StreamState::new();
    let choice = StreamChoice {
        delta: StreamDelta {
            content: None,
            reasoning_content: Some("thinking".to_owned()),
            tool_calls: None,
        },
    };
    let mut updates = Vec::new();

    apply_stream_choice(&mut state, choice, &mut |text, thinking| {
        updates.push((text.to_owned(), thinking.map(str::to_owned)));
    });

    assert_eq!(state.text, "");
    assert_eq!(state.thinking.as_deref(), Some("thinking"));
    assert_eq!(updates, vec![("".to_owned(), Some("thinking".to_owned()))]);
}

#[test]
fn stream_choice_accepts_reasoning_alias() {
    let chunk: StreamChunk = serde_json::from_value(serde_json::json!({
        "choices": [{
            "delta": {
                "reasoning": "alias thinking"
            }
        }]
    }))
    .expect("chunk");

    let choice = chunk.choices.into_iter().next().expect("choice");
    assert_eq!(
        choice.delta.reasoning_content.as_deref(),
        Some("alias thinking")
    );
}

#[test]
fn stream_chunk_reads_llama_cpp_cache_stats() {
    let chunk: StreamChunk = serde_json::from_value(serde_json::json!({
        "choices": [],
        "tokens_cached": 42,
        "tokens_evaluated": 100,
    }))
    .expect("chunk");

    assert_eq!(chunk.tokens_cached, Some(42));
    assert_eq!(chunk.tokens_evaluated, Some(100));
}
