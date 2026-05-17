//! ChatGPT/Codex Responses API client.
//!
//! Endpoint: `POST {base_url}/codex/responses`
//!
//! SSE streaming with `response.output_text.delta` events.
//!
//! When the resolved config advertises `supports_websocket`, the provider
//! loop routes Responses turns through the [`ws`] module
//! instead — same wire envelope, persistent connection, per-session
//! pooling. This module's HTTP+SSE path is kept as the universal
//! fallback (and as the only transport for endpoints that don't
//! support WS).

use std::io::BufRead;
use std::path::PathBuf;

use serde::Serialize;
use tau_proto::{ContentPart, ContextItem, ContextRole, ToolResultStatus};

use crate::common::{
    LlmError, PromptPayload, StreamState, cbor_to_json, effort_wire, mix_originator_into_cache_key,
    prompt_cache_key_for,
};

pub(crate) mod pool;
pub(crate) mod ws;
pub(crate) mod ws_runtime;

/// Which ChatGPT/Codex Responses surface a model is served through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesSurface {
    /// ChatGPT/Codex OAuth-backed `/codex/responses` endpoint.
    ChatGpt,
}

impl ResponsesSurface {
    fn responses_url(self, base_url: &str) -> String {
        let _ = self;
        let base = base_url.trim_end_matches('/');
        format!("{base}/codex/responses")
    }

    fn compact_url(self, base_url: &str) -> String {
        let _ = self;
        let base = base_url.trim_end_matches('/');
        format!("{base}/codex/responses/compact")
    }

    fn store_value(self) -> bool {
        let _ = self;
        false
    }
}

/// Config for the ChatGPT/Codex Responses API.
#[derive(Clone, Debug)]
pub struct ResponsesConfig {
    /// Responses API surface used for endpoint and request-body differences.
    pub surface: ResponsesSurface,
    /// Base URL for the selected surface, without the final Responses path.
    pub base_url: String,
    /// Bearer credential to send in the `Authorization` header.
    pub api_key: String,
    /// Upstream model id without the Tau provider namespace.
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
    ///    on `response.output_item.done` and forwards it as an ordered
    ///    `ContextItem::Reasoning` in `AgentResponseFinished.output_items`.
    ///
    /// When off, no `include` field is sent and reasoning items are
    /// not captured. Pi calls this "encrypted reasoning replay"; it's
    /// what keeps the model's reasoning continuity intact across a
    /// broken chain (reconnect, fork, fingerprint mismatch) without
    /// having to actually re-derive it from the visible transcript.
    pub supports_encrypted_reasoning: bool,
    /// Whether to attempt a persistent WebSocket transport for this
    /// provider instead of one-shot HTTP+SSE.
    pub supports_websocket: bool,
    /// Whether this provider exposes a standalone compaction endpoint.
    pub supports_compaction: bool,
    /// Whether this provider accepts the `prompt_cache_key` field.
    /// The wire key is derived per `(base_url, session_id)`, then
    /// split by extension name for extension-originated turns.
    pub supports_prompt_cache_key: bool,
}

/// Write the exact Responses request body Tau is about to send upstream.
///
/// This records the full prompt transcript, including tool results. It never
/// writes credentials or request headers. Files are written under the session
/// debug directory:
///
/// `~/.local/state/tau/sessions/<session_id>/debug/provider-requests/`.
pub(crate) fn maybe_debug_write_provider_request(
    session_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    transport: tau_proto::AgentBackendTransport,
) {
    if let Err(error) = debug_write_provider_request(session_prompt_id, config, request, transport)
    {
        tracing::warn!(
            target: crate::LOG_TARGET,
            session_id = %request.session_id,
            session_prompt_id,
            "failed to write provider request debug log: {error}",
        );
    }
}

pub(crate) fn debug_provider_request_dir(session_id: &str) -> Option<PathBuf> {
    let state = tau_config::settings::state_dir()?;
    Some(
        tau_config::settings::sessions_dir_of(&state)
            .join(session_id)
            .join("debug")
            .join("provider-requests"),
    )
}

fn debug_write_provider_request(
    session_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    transport: tau_proto::AgentBackendTransport,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(dir) = debug_provider_request_dir(request.session_id) else {
        return Ok(());
    };
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let transport_label = match transport {
        tau_proto::AgentBackendTransport::HttpSse => "http-sse",
        tau_proto::AgentBackendTransport::Websocket => "websocket",
    };
    let path = dir.join(format!(
        "{ts}-{session_prompt_id}-{transport_label}-request.json"
    ));
    let body = match transport {
        tau_proto::AgentBackendTransport::HttpSse => {
            serde_json::to_value(build_request(config, request))?
        }
        tau_proto::AgentBackendTransport::Websocket => {
            serde_json::to_value(build_ws_envelope(config, request))?
        }
    };
    let metadata = serde_json::json!({
        "session_id": request.session_id,
        "session_prompt_id": session_prompt_id,
        "transport": transport_label,
        "backend": "responses",
        "model": config.model_id,
        "previous_response": request.previous_response.as_ref().map(|p| serde_json::json!({
            "id": p.id,
            "next_item_index": p.next_item_index,
            "transport": p.transport,
        })),
        "context_item_count": request.context_items.len(),
        "tool_count": request.tools.len(),
        "tool_choice": request.tool_choice,
        "body": body,
    });
    std::fs::write(path, serde_json::to_vec_pretty(&metadata)?)?;
    Ok(())
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
    session_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<StreamState, LlmError> {
    let first = responses_stream_once(session_prompt_id, config, request, on_update);
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
        context_items: request.context_items,
        tools: request.tools,
        params: request.params,
        tool_choice: request.tool_choice,
        originator: request.originator,
        session_id: request.session_id,
        share_user_cache_key: false,
    };
    let mut state = responses_stream_once(session_prompt_id, config, &fallback, on_update)?;
    state.stale_chain_fallback = true;
    Ok(state)
}

pub fn responses_compact(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
) -> Result<Vec<String>, LlmError> {
    let url = config.surface.compact_url(&config.base_url);
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
    session_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<StreamState, LlmError> {
    let url = config.surface.responses_url(&config.base_url);

    maybe_debug_write_provider_request(
        session_prompt_id,
        config,
        request,
        tau_proto::AgentBackendTransport::HttpSse,
    );
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
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                state.append_message_delta_at(output_index, delta);
                on_update(&state.text, state.thinking.as_deref());
            }
        }
        "response.output_text.done" => {
            if let Some(text) = event["text"].as_str() {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                state.set_message_text_at(output_index, text);
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
                state
                    .tool_call_at_mut(output_index, tau_proto::ToolType::Function)
                    .arguments_json
                    .push_str(delta);
            }
        }
        "response.function_call_arguments.done" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(arguments) = event["arguments"].as_str() {
                state
                    .tool_call_at_mut(output_index, tau_proto::ToolType::Function)
                    .arguments_json = arguments.to_owned();
            }
        }
        "response.custom_tool_call_input.delta" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(delta) = event["delta"].as_str() {
                state
                    .tool_call_at_mut(output_index, tau_proto::ToolType::Custom)
                    .arguments_json
                    .push_str(delta);
            }
        }
        "response.custom_tool_call_input.done" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(input) = event["input"].as_str() {
                state
                    .tool_call_at_mut(output_index, tau_proto::ToolType::Custom)
                    .arguments_json = input.to_owned();
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                let tool_type = match item["type"].as_str() {
                    Some("function_call") => Some(tau_proto::ToolType::Function),
                    Some("custom_tool_call") => Some(tau_proto::ToolType::Custom),
                    _ => None,
                };
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(tool_type) = tool_type {
                    let call = state.tool_call_at_mut(output_index, tool_type);
                    if let Some(id) = item["call_id"].as_str() {
                        call.id = id.to_owned();
                    }
                    if let Some(name) = item["name"].as_str() {
                        call.name = name.to_owned();
                    }
                    if call.arguments_json.is_empty() {
                        let final_input = match tool_type {
                            tau_proto::ToolType::Function => item["arguments"].as_str(),
                            tau_proto::ToolType::Custom => item["input"].as_str(),
                        };
                        if let Some(final_input) = final_input {
                            call.arguments_json = final_input.to_owned();
                        }
                    }
                }
                if item["type"].as_str() == Some("message") {
                    state.set_message_phase_at(output_index, parse_phase_from_item(item));
                    if event_type == "response.output_item.done" {
                        if let Some(text) = message_text_from_output_item(item) {
                            let previous_text = state.text.clone();
                            state.set_message_text_at(output_index, &text);
                            if state.text != previous_text {
                                on_update(&state.text, state.thinking.as_deref());
                            }
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
                    state.set_reasoning_item_json_at(output_index, &item.to_string());
                }
            }
        }
        "response.completed" | "response.done" => {
            state.provider_terminal_event = Some(event.clone());
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
    let (input_items, previous_response_id): (&[ContextItem], Option<String>) =
        match request.previous_response {
            Some(prev) if prev.next_item_index <= request.context_items.len() => (
                &request.context_items[prev.next_item_index..],
                Some(prev.id.to_owned()),
            ),
            _ => (request.context_items, None),
        };

    let input = build_input_items(config, input_items);

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
        // ChatGPT/Codex rejects `store: true`, even when chaining with a
        // `previous_response_id`, so the provider owns this endpoint quirk.
        store: Some(config.surface.store_value()),
        tools,
        tool_choice,
        reasoning,
        text,
        include,
        prompt_cache_key,
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
            context_items: request.context_items,
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
    body.service_tier = None;
    body.include.clear();
    body
}

fn build_input_items(
    config: &ResponsesConfig,
    input_items: &[ContextItem],
) -> Vec<serde_json::Value> {
    let mut input = Vec::new();
    for item in input_items {
        convert_context_item(item, config.supports_phase, &mut input);
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

/// Extracts the assistant-phase label off a Responses-API `output_item.*`
/// item, when the item is an assistant `message`
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

fn message_text_from_output_item(item: &serde_json::Value) -> Option<String> {
    let mut text = String::new();

    for part in item
        .get("content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let is_text_part = matches!(
            part.get("type").and_then(serde_json::Value::as_str),
            Some("output_text") | Some("text")
        );
        if is_text_part {
            if let Some(part_text) = part.get("text").and_then(serde_json::Value::as_str) {
                text.push_str(part_text);
            }
        }
    }

    if text.is_empty() { None } else { Some(text) }
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

fn convert_context_item(
    item: &ContextItem,
    supports_phase: bool,
    out: &mut Vec<serde_json::Value>,
) {
    match item {
        ContextItem::Message(msg) if msg.role == ContextRole::User => {
            // Collect text blocks into one user message, emit tool results separately.
            let mut text_items: Vec<serde_json::Value> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentPart::Text { text } => {
                        text_items.push(serde_json::json!({
                            "type": "input_text",
                            "text": text,
                        }));
                    }
                }
            }
            if !text_items.is_empty() {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": text_items,
                }));
            }
        }
        ContextItem::Message(msg) if msg.role == ContextRole::Assistant => {
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
                    ContentPart::Text { text } => {
                        text_parts.push(text.clone());
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
        ContextItem::ToolCall(call) => {
            let id_str = call.call_id.as_str();
            match call.tool_type {
                tau_proto::ToolType::Function => {
                    let args_json = cbor_to_json(&call.arguments);
                    let fc_id = if id_str.starts_with("fc_") {
                        id_str.to_owned()
                    } else {
                        format!("fc_{id_str}")
                    };
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "id": fc_id,
                        "call_id": id_str,
                        "name": encode_tool_name(call.name.as_str()),
                        "arguments": serde_json::to_string(&args_json).unwrap_or_default(),
                    }));
                }
                tau_proto::ToolType::Custom => {
                    let custom_id = if id_str.starts_with("ctc_") {
                        id_str.to_owned()
                    } else {
                        format!("ctc_{id_str}")
                    };
                    let input = match &call.arguments {
                        tau_proto::CborValue::Text(text) => text.clone(),
                        other => serde_json::to_string(&cbor_to_json(other)).unwrap_or_default(),
                    };
                    out.push(serde_json::json!({
                        "type": "custom_tool_call",
                        "id": custom_id,
                        "call_id": id_str,
                        "name": encode_tool_name(call.name.as_str()),
                        "input": input,
                    }));
                }
            }
        }
        ContextItem::ToolResult(result) => {
            let output = match &result.status {
                ToolResultStatus::Success => match &result.output {
                    tau_proto::CborValue::Text(text) => text.clone(),
                    other => serde_json::to_string(&cbor_to_json(other)).unwrap_or_default(),
                },
                ToolResultStatus::Error { message } => format!("ERROR: {message}"),
                ToolResultStatus::Cancelled { reason } => format!("CANCELLED: {reason}"),
            };
            let output_type = match result.tool_type {
                tau_proto::ToolType::Function => "function_call_output",
                tau_proto::ToolType::Custom => "custom_tool_call_output",
            };
            out.push(serde_json::json!({
                "type": output_type,
                "call_id": result.call_id,
                "output": output,
            }));
        }
        ContextItem::Reasoning(item) => {
            out.push(cbor_to_json(&item.0));
        }
        ContextItem::Compaction(item) | ContextItem::UnknownProviderItem(item) => {
            out.push(cbor_to_json(&item.0));
        }
        ContextItem::Message(_) => {}
    }
}

#[cfg(test)]
mod tests;
