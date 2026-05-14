//! OpenAI Codex Responses API client (ChatGPT subscriptions).
//!
//! Endpoint: `POST {base_url}/codex/responses`
//! SSE streaming with `response.output_text.delta` events.
//!
//! When the resolver advertises `supports_websocket`, the harness's
//! agent loop routes Responses turns through the [`ws`] module
//! instead — same wire envelope, persistent connection, per-session
//! pooling. This module's HTTP+SSE path is kept as the universal
//! fallback (and as the only transport for endpoints that don't
//! support WS).

use std::io::BufRead;

use serde::Serialize;
use tau_proto::{ContentBlock, ConversationMessage, ConversationRole};

use crate::common::{
    LlmError, PromptPayload, StreamState, ToolCallAccumulator, cbor_to_json, effort_wire,
    mix_originator_into_cache_key, prompt_cache_key_for,
};

pub(crate) mod pool;
pub(crate) mod ws;
pub(crate) mod ws_runtime;

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
    ///
    /// When off, no `phase` field is sent or parsed.
    pub supports_phase: bool,
    /// Whether the provider returns `reasoning` output items with a
    /// replayable `encrypted_content` field when the request body
    /// asks for `include: ["reasoning.encrypted_content"]`. Currently
    /// the Codex Responses backend on `gpt-5.3-codex+`. When on:
    /// 1. The request body sets `include: ["reasoning.encrypted_content"]` so
    ///    the model's reasoning output items carry the encrypted blob the
    ///    harness can replay verbatim.
    /// 2. The SSE/WS parser captures each `reasoning` output item's full JSON
    ///    on `response.output_item.done` and forwards it on
    ///    [`tau_proto::AgentResponseFinished::reasoning_items`].
    ///
    /// When off, no `include` field is sent and reasoning items are
    /// not captured. Pi calls this "encrypted reasoning replay"; it's
    /// what keeps the model's reasoning continuity intact across a
    /// broken chain (reconnect, fork, fingerprint mismatch) without
    /// having to actually re-derive it from the visible transcript.
    pub supports_encrypted_reasoning: bool,
    /// Whether to attempt a persistent WebSocket transport for this
    /// provider instead of one-shot HTTP+SSE. See
    /// [`tau_config::settings::ProviderCompat::supports_websocket`].
    pub supports_websocket: bool,
    /// Whether this provider exposes a standalone compaction endpoint.
    pub supports_compaction: bool,
    /// Whether this provider accepts the `prompt_cache_key` field.
    /// The wire key is derived per `(base_url, session_id)`, then
    /// split by extension name for extension-originated turns.
    pub supports_prompt_cache_key: bool,
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
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<StreamState, LlmError> {
    let first = responses_stream_once(config, request, on_update);
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
        compacted_input_items: request.compacted_input_items,
        tools: request.tools,
        params: request.params,
        tool_choice: request.tool_choice,
        originator: request.originator,
        session_id: request.session_id,
        share_user_cache_key: false,
    };
    responses_stream_once(config, &fallback, on_update)
}

pub fn responses_compact(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
) -> Result<Vec<String>, LlmError> {
    let url = format!(
        "{}/codex/responses/compact",
        config.base_url.trim_end_matches('/')
    );
    let body = build_compact_request(config, request);
    let body_str = serde_json::to_string(&body).map_err(LlmError::Json)?;

    let mut req = tau_provider::oauth::proxy_agent()
        .post(&url)
        .set("Content-Type", "application/json")
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
    let value: serde_json::Value =
        serde_json::from_reader(response.into_reader()).map_err(LlmError::Json)?;
    let output = value
        .get("output")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| LlmError::HttpStatus(0, "compaction response missing output".to_owned()))?;
    Ok(output.iter().map(serde_json::Value::to_string).collect())
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

        if apply_event(&mut state, &event, on_update)? {
            break;
        }
    }

    Ok(state)
}

/// Apply one decoded `response.*` event from the upstream stream to
/// `state`. Returns `Ok(true)` when the event terminates the stream
/// (`response.completed` / `response.done`), `Ok(false)` to keep
/// reading, or an error when the server signaled a model-side
/// failure that should be surfaced as `LlmError`.
///
/// Shared between the HTTP+SSE and WebSocket transports — both
/// decode a single JSON event and hand it here. The WS docs state
/// "server events and ordering match the existing Responses
/// streaming event model", so the parse rules are identical.
pub(crate) fn apply_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<bool, LlmError> {
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
                        tool_type: tau_proto::ToolType::Function,
                        arguments_json: String::new(),
                    });
                }
                state.tool_calls[output_index]
                    .arguments_json
                    .push_str(delta);
            }
        }
        "response.custom_tool_call_input.delta" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(delta) = event["delta"].as_str() {
                while state.tool_calls.len() <= output_index {
                    state.tool_calls.push(ToolCallAccumulator {
                        id: String::new(),
                        name: String::new(),
                        tool_type: tau_proto::ToolType::Custom,
                        arguments_json: String::new(),
                    });
                }
                let call = &mut state.tool_calls[output_index];
                call.tool_type = tau_proto::ToolType::Custom;
                call.arguments_json.push_str(delta);
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                let tool_type = match item["type"].as_str() {
                    Some("function_call") => Some(tau_proto::ToolType::Function),
                    Some("custom_tool_call") => Some(tau_proto::ToolType::Custom),
                    _ => None,
                };
                if let Some(tool_type) = tool_type {
                    let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                    while state.tool_calls.len() <= output_index {
                        state.tool_calls.push(ToolCallAccumulator {
                            id: String::new(),
                            name: String::new(),
                            tool_type,
                            arguments_json: String::new(),
                        });
                    }
                    state.tool_calls[output_index].tool_type = tool_type;
                    if let Some(id) = item["call_id"].as_str() {
                        state.tool_calls[output_index].id = id.to_owned();
                    }
                    if let Some(name) = item["name"].as_str() {
                        state.tool_calls[output_index].name = name.to_owned();
                    }
                    if state.tool_calls[output_index].arguments_json.is_empty() {
                        let final_input = match tool_type {
                            tau_proto::ToolType::Function => item["arguments"].as_str(),
                            tau_proto::ToolType::Custom => item["input"].as_str(),
                        };
                        if let Some(final_input) = final_input {
                            state.tool_calls[output_index].arguments_json = final_input.to_owned();
                        }
                    }
                }
                // Capture reasoning items only on `output_item.done`,
                // not on `added` — the `added` event arrives before
                // any summary parts/encrypted content stream in, so
                // its payload is just a stub. `done` carries the full
                // item (id + encrypted_content + summary) the harness
                // needs to replay verbatim on the next turn.
                //
                // The whole item is stashed as opaque JSON so a future
                // wire-format change (extra fields, schema rev) round-
                // trips without code changes — same Pi-style blob the
                // harness re-emits on full-transcript replay.
                //
                // An item without `encrypted_content` is unreplayable:
                // the server stores reasoning only for `store: true`
                // requests, and Codex forces `store: false`, so a bare
                // `rs_…` id in a later turn's `input[]` triggers
                // `Item with id 'rs_…' not found` and an 8-attempt
                // retry loop. Skip those — losing reasoning continuity
                // on this turn is better than poisoning the chain.
                if event_type == "response.output_item.done"
                    && item["type"].as_str() == Some("reasoning")
                    && item["encrypted_content"].is_string()
                {
                    state.reasoning_items.push(item.to_string());
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
                    .or_else(|| event["usage"]["input_tokens_details"]["cached_tokens"].as_u64());
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
            return Ok(true);
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
            // Preserve the error code alongside the message so the
            // retry classifier can distinguish a transient transport
            // hiccup from an account-level cap (usage limit, rate
            // limit, quota) — the latter must not be retried.
            //
            // The OpenAI Responses streaming `error` event uses
            // `code` at the top level (e.g. `code:
            // "rate_limit_exceeded"`); some Codex variants nest an
            // `error.code` or older-style `error.type`. We check
            // all three so an upstream wording drift on one path
            // doesn't silently re-enable the futile retry loop on
            // an account cap. The `(type=...)` suffix is a stable
            // substring contract matched by `LlmError::retry_after`
            // and `pool::is_recoverable_ws_error`.
            let error_code = event["error"]["code"]
                .as_str()
                .or_else(|| event["code"].as_str())
                .or_else(|| event["error"]["type"].as_str());
            let body = match error_code {
                Some(code) => format!("stream error: {detail} (type={code})"),
                None => format!("stream error: {detail}"),
            };
            return Err(LlmError::HttpStatus(0, body));
        }
        _ => {}
    }
    Ok(false)
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
    /// `Some(true)` for HTTP+SSE transport — the only mode where the
    /// `stream` flag actually toggles framing. `None` on the WS
    /// transport, where the WS guide explicitly notes "transport-
    /// specific fields like `stream` and `background` are not used".
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Always `Some(false)` on the ChatGPT Codex Responses endpoint —
    /// it rejects `store: true` even when chaining (see
    /// `build_request`). Omitted for the standalone compaction endpoint,
    /// which rejects the field entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
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
    /// Optional opt-ins for richer response payloads. Currently only
    /// used to flip on `"reasoning.encrypted_content"`, which makes
    /// the model return an opaque per-`reasoning`-item blob the
    /// harness persists and replays on later turns. Omitted entirely
    /// when nothing's asked for so older endpoints don't trip on an
    /// unknown field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<&'static str>,
    /// Optional upstream service tier (`fast` for Fast mode, `flex` for
    /// lower-priority service).
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'static str>,
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

    let input = build_input_items(
        config,
        request.messages,
        input_messages,
        if previous_response_id.is_none() {
            request.compacted_input_items
        } else {
            &[]
        },
    );

    let tools: Vec<serde_json::Value> = request.tools.iter().map(convert_tool_definition).collect();

    let tool_choice = match (request.tool_choice, tools.is_empty()) {
        // Harness-forced no-tools-this-turn: explicit `none` works
        // whether or not tools are declared (and is the whole point
        // of this branch — tools stay declared so the cache prefix
        // matches, the model is just told not to call them).
        (tau_proto::ToolChoice::None, _) => Some("none".to_owned()),
        // Default: only mention `tool_choice` when there are actual
        // tools — emitting `"auto"` on an empty list bumps the
        // request body for no reason and some endpoints reject it.
        (tau_proto::ToolChoice::Auto, false) => Some("auto".to_owned()),
        (tau_proto::ToolChoice::Auto, true) => None,
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
    let prompt_cache_key = config
        .supports_prompt_cache_key
        .then(|| prompt_cache_key_for(&config.base_url, request.session_id));
    let prompt_cache_key = mix_originator_into_cache_key(
        prompt_cache_key.as_deref(),
        request.originator,
        request.share_user_cache_key,
    );
    let prompt_cache_retention = config
        .prompt_cache_retention
        .map(tau_config::settings::PromptCacheRetention::as_wire);
    let include: Vec<&'static str> = if config.supports_encrypted_reasoning {
        vec!["reasoning.encrypted_content"]
    } else {
        Vec::new()
    };

    ResponsesRequest {
        model: config.model_id.clone(),
        instructions,
        input,
        // HTTP path always streams; WS path overrides this back to
        // `None` via [`ws_envelope`] before serializing.
        stream: Some(true),
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
        store: Some(false),
        tools,
        tool_choice,
        reasoning,
        text,
        include,
        prompt_cache_key,
        prompt_cache_retention,
        service_tier: request
            .params
            .service_tier
            .map(tau_proto::ServiceTier::as_wire),
        previous_response_id,
    }
}

fn build_compact_request(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
) -> ResponsesRequest {
    let mut body = build_request(
        config,
        &PromptPayload {
            system_prompt: request.system_prompt,
            messages: request.messages,
            compacted_input_items: request.compacted_input_items,
            tools: request.tools,
            params: request.params,
            tool_choice: request.tool_choice,
            previous_response: None,
            originator: request.originator,
            share_user_cache_key: request.share_user_cache_key,
            session_id: request.session_id,
        },
    );
    body.stream = None;
    body.previous_response_id = None;
    body.store = None;
    body.tool_choice = None;
    body.prompt_cache_key = None;
    body.prompt_cache_retention = None;
    body.service_tier = None;
    body.include.clear();
    body
}

fn build_input_items(
    config: &ResponsesConfig,
    messages: &[ConversationMessage],
    input_messages: &[ConversationMessage],
    compacted_input_items: &[String],
) -> Vec<serde_json::Value> {
    let mut input: Vec<serde_json::Value> = compacted_input_items
        .iter()
        .filter_map(|item| serde_json::from_str(item).ok())
        .collect();
    // Track call_id -> tool type across the full conversation, not
    // just the suffix we send on a chained request. Tool results can
    // remain in the suffix after their matching ToolUse was anchored
    // by `previous_response_id`.
    let mut tool_types_by_call_id = collect_tool_types(messages);
    for msg in input_messages {
        convert_message(
            msg,
            config.supports_phase,
            &mut tool_types_by_call_id,
            &mut input,
        );
    }
    input
}

/// WebSocket-side wrapper around a [`ResponsesRequest`]. The OpenAI
/// WS guide requires every client message to carry `type:
/// "response.create"` at the top level, while HTTP+SSE has no
/// envelope. `#[serde(flatten)]` keeps the body shape identical
/// across the two transports so request-build tests don't need a
/// separate fixture for each.
#[derive(Serialize)]
pub(crate) struct WsResponseCreate {
    #[serde(rename = "type")]
    ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    generate: Option<bool>,
    #[serde(flatten)]
    body: ResponsesRequest,
}

/// Build the JSON envelope to send over a WebSocket text frame for
/// one turn. Reuses [`build_request`] for the body — the only deltas
/// vs. the HTTP body are (a) the top-level `type` tag and (b)
/// dropping `stream` (transport-implicit on WS, per the WS guide).
pub(crate) fn build_ws_envelope(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
) -> WsResponseCreate {
    let mut body = build_request(config, request);
    body.stream = None;
    WsResponseCreate {
        ty: "response.create",
        generate: None,
        body,
    }
}

/// Stable fingerprint of the request fields that must match for a
/// `generate:false` prewarm response to be usable as a
/// `previous_response_id` anchor. The prompt `input` and chain id are
/// intentionally blanked: the next real turn is allowed to extend the
/// prewarmed input prefix, and the prewarm itself never has a prior
/// chain id.
pub(crate) fn ws_chain_fingerprint(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
) -> Result<String, LlmError> {
    let mut body = build_request(config, request);
    body.input.clear();
    body.previous_response_id = None;
    serde_json::to_string(&body).map_err(LlmError::Json)
}

/// Build a non-generating WebSocket envelope for provider-side
/// prompt-cache prewarm. Normal turns must keep `generate` omitted;
/// only this path serializes `generate: false`.
pub(crate) fn build_ws_prewarm_envelope(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
) -> WsResponseCreate {
    let mut body = build_request(config, request);
    body.stream = None;
    body.previous_response_id = None;
    WsResponseCreate {
        ty: "response.create",
        generate: Some(false),
        body,
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

fn convert_tool_definition(tool: &tau_proto::ToolDefinition) -> serde_json::Value {
    let model_visible_name = tool.model_visible_name.as_ref().unwrap_or(&tool.name);
    match tool.tool_type {
        tau_proto::ToolType::Function => {
            let mut wire = serde_json::json!({
                "type": "function",
                "name": encode_tool_name(model_visible_name),
                "strict": serde_json::Value::Null,
            });
            if let Some(ref desc) = tool.description {
                wire["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref params) = tool.parameters {
                wire["parameters"] = params.clone();
            }
            wire
        }
        tau_proto::ToolType::Custom => {
            let mut wire = serde_json::json!({
                "type": "custom",
                "name": encode_tool_name(model_visible_name),
            });
            if let Some(ref desc) = tool.description {
                wire["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref format) = tool.format {
                wire["format"] = serialize_tool_format(format);
            }
            wire
        }
    }
}

fn collect_tool_types(
    messages: &[ConversationMessage],
) -> std::collections::HashMap<String, tau_proto::ToolType> {
    let mut tool_types_by_call_id = std::collections::HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, tool_type, .. } = block {
                tool_types_by_call_id.insert(id.as_str().to_owned(), *tool_type);
            }
        }
    }
    tool_types_by_call_id
}

fn serialize_tool_format(format: &tau_proto::ToolFormat) -> serde_json::Value {
    match format {
        tau_proto::ToolFormat::Text => serde_json::json!({
            "type": "text",
        }),
        tau_proto::ToolFormat::Grammar { syntax, definition } => serde_json::json!({
            "type": "grammar",
            "syntax": match syntax {
                tau_proto::ToolGrammarSyntax::Lark => "lark",
                tau_proto::ToolGrammarSyntax::Regex => "regex",
            },
            "definition": definition,
        }),
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
    tool_types_by_call_id: &mut std::collections::HashMap<String, tau_proto::ToolType>,
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
                        let output_type = match tool_types_by_call_id.get(tool_use_id.as_str()) {
                            Some(tau_proto::ToolType::Custom) => "custom_tool_call_output",
                            _ => "function_call_output",
                        };
                        out.push(serde_json::json!({
                            "type": output_type,
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                    }
                    ContentBlock::ToolUse { .. } => {}
                    // Reasoning items are assistant-role artifacts and
                    // never appear on user messages in practice; emit
                    // nothing on this match for forward-compatibility
                    // with a malformed/legacy persisted message.
                    ContentBlock::Reasoning { .. } => {}
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
                        id,
                        name,
                        tool_type,
                        input,
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
                        let id_str = id.as_str();
                        tool_types_by_call_id.insert(id_str.to_owned(), *tool_type);
                        match tool_type {
                            tau_proto::ToolType::Function => {
                                let args_json = cbor_to_json(input);
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
                            tau_proto::ToolType::Custom => {
                                let custom_id = if id_str.starts_with("ctc_") {
                                    id_str.to_owned()
                                } else {
                                    format!("ctc_{id_str}")
                                };
                                let input = match input {
                                    tau_proto::CborValue::Text(text) => text.clone(),
                                    other => serde_json::to_string(&cbor_to_json(other))
                                        .unwrap_or_default(),
                                };
                                out.push(serde_json::json!({
                                    "type": "custom_tool_call",
                                    "id": custom_id,
                                    "call_id": id_str,
                                    "name": encode_tool_name(name.as_str()),
                                    "input": input,
                                }));
                            }
                        }
                    }
                    ContentBlock::ToolResult { .. } => {}
                    ContentBlock::Reasoning { item } => {
                        // Reasoning items are top-level `input[]`
                        // entries on the Codex Responses API — same
                        // structural slot as `message` and
                        // `function_call`, never nested inside an
                        // assistant message. Flush any accumulated
                        // text first so the message item lands after
                        // the reasoning, matching the order the
                        // server emitted them; then re-emit the raw
                        // JSON the agent captured (id + encrypted
                        // content + summary) verbatim. A parse
                        // failure silently drops the item — same
                        // outcome as the chain breaking, no harm
                        // beyond losing reasoning continuity.
                        if !text_parts.is_empty() {
                            let mut flushed = serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": text_parts.join("\n"),
                                    "annotations": [],
                                }],
                            });
                            if let Some(phase) = phase_wire {
                                flushed["phase"] = serde_json::Value::String(phase.to_owned());
                            }
                            out.push(flushed);
                            text_parts.clear();
                        }
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(item) {
                            out.push(value);
                        }
                    }
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
