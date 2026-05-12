//! OpenAI Codex Responses API client (ChatGPT subscriptions).
//!
//! Endpoint: `POST {base_url}/codex/responses`
//! SSE streaming with `response.output_text.delta` events.

use std::io::BufRead;

use serde::Serialize;
use tau_proto::{ContentBlock, ConversationMessage, ConversationRole};

use crate::common::{
    LlmError, PromptPayload, StreamState, ToolCallAccumulator, cbor_to_json, effort_wire,
};

/// Config for the Codex Responses API.
#[derive(Clone, Debug)]
pub struct ResponsesConfig {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    /// `chatgpt-account-id` header extracted from JWT.
    pub account_id: Option<String>,
    /// Whether the provider's API accepts a `reasoning.effort` field.
    pub supports_reasoning_effort: bool,
    /// Whether the provider's API accepts `reasoning.summary` and
    /// streams `response.reasoning_summary_text.*` events.
    pub supports_reasoning_summary: bool,
    /// Whether the provider's API accepts a `text.verbosity` field
    /// (OpenAI Responses on GPT-5+).
    pub supports_verbosity: bool,
    /// Whether the provider's API accepts (and the model emits) the
    /// `phase` field on assistant `message` items
    /// (`commentary` / `final_answer`). When on:
    /// 1. The Responses backend stamps `phase` on every outgoing assistant
    ///    message, defaulting to `final_answer` when the stored history doesn't
    ///    carry one (matches OpenAI's deployment-checklist guidance for
    ///    backwards compatibility).
    /// 2. The SSE parser captures `phase` off the assistant `message` item so
    ///    the harness can persist it.
    /// When off, no `phase` field is sent or parsed.
    pub supports_phase: bool,
    /// Routing key sent as `prompt_cache_key`. See
    /// `openai::prompt_cache_key` for the derivation rationale.
    pub prompt_cache_key: Option<String>,
    /// Provider-side prompt cache retention policy, when configured.
    pub prompt_cache_retention: Option<tau_config::settings::PromptCacheRetention>,
}

/// Calls the Codex Responses API with SSE streaming.
///
/// `on_update` is invoked on each visible delta with `(text,
/// thinking)`, where `thinking` is the accumulated reasoning summary
/// the provider has streamed so far (or `None` if no summary
/// content has arrived yet).
///
/// Stateful-chain fallback: when `request.previous_response` was set
/// but the upstream rejects the prior `response_id` (server-side
/// expiry, evicted state), this retries once with the full transcript
/// instead of the delta. The fallback is invisible to the caller — a
/// successful result looks identical to a chain hit, just with a
/// larger request body. Only triggered for HTTP 4xx whose body
/// mentions `previous_response`; transient 5xx / network errors
/// surface to the harness retry layer unchanged.
pub fn responses_stream(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    mut on_update: impl FnMut(&str, Option<&str>),
) -> Result<StreamState, LlmError> {
    let first = responses_stream_once(config, request, &mut on_update);
    if request.previous_response.is_none() {
        return first;
    }
    let Err(error) = first else {
        return first;
    };
    if !is_stale_chain_error(&error) {
        return Err(error);
    }
    tracing::info!(
        target: crate::LOG_TARGET,
        "Responses chain rejected by upstream; retrying with full replay"
    );
    let fallback = PromptPayload {
        previous_response: None,
        system_prompt: request.system_prompt,
        messages: request.messages,
        tools: request.tools,
        params: request.params,
    };
    responses_stream_once(config, &fallback, &mut on_update)
}

/// True for the narrow class of 4xx errors that say the prior
/// `previous_response_id` we sent is no longer valid (expired,
/// evicted, never existed). Matched on body substrings rather than a
/// brittle JSON-shape contract — the provider's error envelope has
/// changed shape before, and a missed match here just means the user
/// sees the error instead of an auto-retry.
fn is_stale_chain_error(error: &LlmError) -> bool {
    let LlmError::HttpStatus(code, body) = error else {
        return false;
    };
    if !(400..500).contains(code) {
        return false;
    }
    body.contains("previous_response") || body.contains("response not found")
}

fn responses_stream_once(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<StreamState, LlmError> {
    let url = format!("{}/codex/responses", config.base_url.trim_end_matches('/'));

    let body = build_request(config, request);
    let body_str = serde_json::to_string(&body).map_err(LlmError::Json)?;

    let mut req = tau_provider::oauth::proxy_agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .set("Authorization", &format!("Bearer {}", config.api_key))
        .set("OpenAI-Beta", "responses=experimental");

    if let Some(ref account_id) = config.account_id {
        req = req.set("chatgpt-account-id", account_id);
    }

    let response = req.send_string(&body_str).map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            LlmError::HttpStatus(code, body)
        }
        other => LlmError::Http(Box::new(other)),
    })?;

    let reader = std::io::BufReader::new(response.into_reader());
    let mut state = StreamState::new();

    for line in reader.lines() {
        let line = line.map_err(LlmError::Io)?;

        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };

        if data == "[DONE]" {
            break;
        }

        let event: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = event["type"].as_str().unwrap_or("");

        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    state.text.push_str(delta);
                    on_update(&state.text, state.thinking.as_deref());
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    state
                        .thinking
                        .get_or_insert_with(String::new)
                        .push_str(delta);
                    on_update(&state.text, state.thinking.as_deref());
                }
            }
            "response.reasoning_summary_part.added" => {
                // Each summary part is a separate paragraph. Insert a
                // blank line between parts so consecutive paragraphs
                // are visually separated.
                if let Some(thinking) = state.thinking.as_mut() {
                    if !thinking.is_empty() && !thinking.ends_with("\n\n") {
                        thinking.push_str("\n\n");
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(delta) = event["delta"].as_str() {
                    while state.tool_calls.len() <= output_index {
                        state.tool_calls.push(ToolCallAccumulator {
                            id: String::new(),
                            name: String::new(),
                            arguments_json: String::new(),
                        });
                    }
                    state.tool_calls[output_index]
                        .arguments_json
                        .push_str(delta);
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    if item["type"].as_str() == Some("function_call") {
                        let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                        while state.tool_calls.len() <= output_index {
                            state.tool_calls.push(ToolCallAccumulator {
                                id: String::new(),
                                name: String::new(),
                                arguments_json: String::new(),
                            });
                        }
                        if let Some(id) = item["call_id"].as_str() {
                            state.tool_calls[output_index].id = id.to_owned();
                        }
                        if let Some(name) = item["name"].as_str() {
                            state.tool_calls[output_index].name = name.to_owned();
                        }
                    }
                    if state.phase.is_none() {
                        state.phase = parse_phase_from_item(item);
                    }
                }
            }
            "response.completed" | "response.done" => {
                if state.input_tokens.is_none() {
                    state.input_tokens = event
                        .get("response")
                        .and_then(|response| response["usage"]["input_tokens"].as_u64())
                        .or_else(|| event["usage"]["input_tokens"].as_u64());
                }
                if state.cached_tokens.is_none() {
                    state.cached_tokens = event
                        .get("response")
                        .and_then(|response| {
                            response["usage"]["input_tokens_details"]["cached_tokens"].as_u64()
                        })
                        .or_else(|| {
                            event["usage"]["input_tokens_details"]["cached_tokens"].as_u64()
                        });
                }
                if state.output_tokens.is_none() {
                    state.output_tokens = event
                        .get("response")
                        .and_then(|response| response["usage"]["output_tokens"].as_u64())
                        .or_else(|| event["usage"]["output_tokens"].as_u64());
                }
                if state.response_id.is_none() {
                    state.response_id = event
                        .get("response")
                        .and_then(|response| response["id"].as_str())
                        .or_else(|| event["id"].as_str())
                        .map(str::to_owned);
                }
                if state.phase.is_none() {
                    // Some providers only surface `phase` on the
                    // terminal `response.completed` envelope, not on
                    // per-item events. Scan `response.output[]` as a
                    // fallback so we capture whatever the model
                    // committed to before the stream ended.
                    state.phase = event
                        .get("response")
                        .and_then(|response| response.get("output"))
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .find_map(parse_phase_from_item);
                }
                break;
            }
            "response.incomplete" => {
                let reason = event
                    .get("response")
                    .and_then(|r| r["incomplete_details"]["reason"].as_str())
                    .unwrap_or("unknown reason");
                return Err(LlmError::HttpStatus(
                    0,
                    format!("response incomplete: {reason}"),
                ));
            }
            "response.failed" => {
                let detail = event
                    .get("response")
                    .and_then(|r| {
                        r["error"]["message"]
                            .as_str()
                            .or_else(|| r["error"]["code"].as_str())
                    })
                    .unwrap_or("unknown error");
                return Err(LlmError::HttpStatus(
                    0,
                    format!("response failed: {detail}"),
                ));
            }
            "error" => {
                let detail = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("unknown error");
                return Err(LlmError::HttpStatus(0, format!("stream error: {detail}")));
            }
            _ => {}
        }
    }

    Ok(state)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<serde_json::Value>,
    stream: bool,
    /// Always `false` on the ChatGPT Codex Responses endpoint — it
    /// rejects `store: true` even when chaining (see `build_request`).
    /// Kept as a serialized field rather than dropped because future
    /// public-API support will need it set to `true` for chained
    /// turns.
    store: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningRequest>,
    /// GPT-5 `text.verbosity` knob. Only set when the provider
    /// advertises `supports_verbosity`; otherwise omitted so older
    /// endpoints don't trip on an unknown field.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<&'static str>,
    /// Stateful-chain mode: points to the prior turn's `response.id`.
    /// When set, the upstream API carries reasoning context across
    /// turns and the request body only needs the *new* input
    /// (`messages[previous_response.message_index..]`). The win is a
    /// smaller request and faster TTFT — the server keeps the prior
    /// reasoning hot rather than re-deriving it from a replayed
    /// transcript. On Codex this works alongside `store: false`; on
    /// the public Responses API it requires `store: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
}

#[derive(Serialize)]
struct ReasoningRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'static str>,
}

#[derive(Serialize)]
struct TextRequest {
    /// `low`/`medium`/`high` — see
    /// <https://developers.openai.com/api/docs/guides/deployment-checklist#set-up-textverbosity>.
    verbosity: &'static str,
}

fn build_request(config: &ResponsesConfig, request: &PromptPayload<'_>) -> ResponsesRequest {
    let instructions = if request.system_prompt.is_empty() {
        None
    } else {
        Some(request.system_prompt.to_owned())
    };

    // Stateful chaining: when the harness supplied a previous
    // response id, slice the messages to just what's new since
    // that response. The OpenAI Responses API picks up the prior
    // conversation from the stored response — replaying its prefix
    // would duplicate it. A defensive cap to `messages.len()` covers
    // the (impossible by harness invariants) case of a stale index.
    let (input_messages, previous_response_id): (&[ConversationMessage], Option<String>) =
        match request.previous_response {
            Some(prev) if prev.message_index <= request.messages.len() => (
                &request.messages[prev.message_index..],
                Some(prev.id.to_owned()),
            ),
            _ => (request.messages, None),
        };

    let mut input = Vec::new();
    for msg in input_messages {
        convert_message(msg, config.supports_phase, &mut input);
    }

    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|t| {
            let mut tool = serde_json::json!({
                "type": "function",
                "name": encode_tool_name(&t.name),
                "strict": serde_json::Value::Null,
            });
            if let Some(ref desc) = t.description {
                tool["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref params) = t.parameters {
                tool["parameters"] = params.clone();
            }
            tool
        })
        .collect();

    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some("auto".to_owned())
    };

    let effort = if config.supports_reasoning_effort {
        effort_wire(request.params.effort)
    } else {
        None
    };
    let summary = if config.supports_reasoning_summary {
        request.params.thinking_summary.as_openai_wire()
    } else {
        None
    };
    let reasoning = if effort.is_some() || summary.is_some() {
        Some(ReasoningRequest { effort, summary })
    } else {
        None
    };
    let text = if config.supports_verbosity {
        Some(TextRequest {
            verbosity: crate::common::verbosity_wire(request.params.verbosity),
        })
    } else {
        None
    };
    let prompt_cache_key = config.prompt_cache_key.clone();
    let prompt_cache_retention = config
        .prompt_cache_retention
        .map(tau_config::settings::PromptCacheRetention::as_wire);

    ResponsesRequest {
        model: config.model_id.clone(),
        instructions,
        input,
        stream: true,
        // ALWAYS `false` on the ChatGPT Codex backend
        // (`chatgpt.com/backend-api/codex/responses`) — it rejects
        // `store: true` with `HTTP 400 {"detail":"Store must be set
        // to false"}` even when `previous_response_id` is also set.
        // The Codex endpoint tracks chains internally; only the
        // public `api.openai.com/v1/responses` endpoint requires
        // `store: true` to use `previous_response_id`. Tau today
        // only routes the Responses backend through Codex, so
        // hardcoding `false` is correct; if/when the public Responses
        // API becomes reachable this needs to become endpoint-aware.
        store: false,
        tools,
        tool_choice,
        reasoning,
        text,
        prompt_cache_key,
        prompt_cache_retention,
        previous_response_id,
    }
}

// ---------------------------------------------------------------------------
// Phase capture
// ---------------------------------------------------------------------------

/// Extracts the assistant-phase label off a Responses-API `output[]`
/// or `output_item.*` item, when the item is an assistant `message`
/// carrying a known `phase` wire string. Returns `None` for items
/// that aren't messages, messages without a `phase` field, or wire
/// strings we don't recognize (forward-compatible: an unknown future
/// value just won't be persisted, rather than panicking).
fn parse_phase_from_item(item: &serde_json::Value) -> Option<tau_proto::MessagePhase> {
    if item.get("type").and_then(serde_json::Value::as_str)? != "message" {
        return None;
    }
    match item.get("phase")?.as_str()? {
        "commentary" => Some(tau_proto::MessagePhase::Commentary),
        "final_answer" => Some(tau_proto::MessagePhase::FinalAnswer),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tool name encoding
// ---------------------------------------------------------------------------

/// Encode tool name for the API: replace non-`[a-zA-Z0-9_-]` with `_`.
fn encode_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Conversation conversion
// ---------------------------------------------------------------------------

fn convert_message(
    msg: &ConversationMessage,
    supports_phase: bool,
    out: &mut Vec<serde_json::Value>,
) {
    match msg.role {
        ConversationRole::User => {
            // Collect text blocks into one user message, emit tool results separately.
            let mut text_items: Vec<serde_json::Value> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_items.push(serde_json::json!({
                            "type": "input_text",
                            "text": text,
                        }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        // Flush any pending text first.
                        if !text_items.is_empty() {
                            out.push(serde_json::json!({
                                "role": "user",
                                "content": text_items,
                            }));
                            text_items = Vec::new();
                        }
                        let output = if *is_error {
                            format!("ERROR: {content}")
                        } else {
                            content.clone()
                        };
                        out.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                    }
                    ContentBlock::ToolUse { .. } => {}
                }
            }
            if !text_items.is_empty() {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": text_items,
                }));
            }
        }
        ConversationRole::Assistant => {
            // Emit tool calls as individual function_call items,
            // text as a message item.
            //
            // `phase` (when the backend supports it): stamp every
            // assistant `message` item we replay. The stored
            // `msg.phase` is preferred; turns from before this
            // field existed (or from non-Codex paths) get the
            // doc-recommended `final_answer` default — the OpenAI
            // deployment checklist explicitly calls this out as the
            // fallback for missing phase on history.
            let phase_wire: Option<&'static str> = if supports_phase {
                Some(
                    msg.phase
                        .unwrap_or(tau_proto::MessagePhase::FinalAnswer)
                        .as_openai_wire(),
                )
            } else {
                None
            };
            let mut text_parts = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        // Emit any pending text first.
                        if !text_parts.is_empty() {
                            let mut item = serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": text_parts.join("\n"),
                                    "annotations": [],
                                }],
                            });
                            if let Some(phase) = phase_wire {
                                item["phase"] = serde_json::Value::String(phase.to_owned());
                            }
                            out.push(item);
                            text_parts.clear();
                        }
                        let args_json = cbor_to_json(input);
                        let id_str = id.as_str();
                        let fc_id = if id_str.starts_with("fc_") {
                            id_str.to_owned()
                        } else {
                            format!("fc_{id_str}")
                        };
                        out.push(serde_json::json!({
                            "type": "function_call",
                            "id": fc_id,
                            "call_id": id_str,
                            "name": encode_tool_name(name.as_str()),
                            "arguments": serde_json::to_string(&args_json).unwrap_or_default(),
                        }));
                    }
                    ContentBlock::ToolResult { .. } => {}
                }
            }
            if !text_parts.is_empty() {
                let mut item = serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": text_parts.join("\n"),
                        "annotations": [],
                    }],
                });
                if let Some(phase) = phase_wire {
                    item["phase"] = serde_json::Value::String(phase.to_owned());
                }
                out.push(item);
            }
        }
    }
}

#[cfg(test)]
mod tests;
