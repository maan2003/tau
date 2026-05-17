//! Types shared by provider-openai's Responses transports.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tau_proto::{
    AgentBackendTransport, AgentTokenUsage, CborValue, ContentPart, ContextItem, ContextRole,
    MessageItem, OpaqueProviderItem, PromptOriginator, SessionId, ToolCallItem, ToolDefinition,
};

/// The parts of a prompt needed by an LLM backend client.
pub struct PromptPayload<'a> {
    pub system_prompt: &'a str,
    pub context_items: &'a [ContextItem],
    pub tools: &'a [ToolDefinition],
    /// Per-prompt model knobs (effort / verbosity / thinking-summary).
    /// Each field is honored only when the backend's config reports
    /// support for the corresponding provider feature.
    pub params: tau_proto::ModelParams,
    /// Whether the model may emit tool-call output on this turn.
    /// `Auto` (default) lets the model decide; `None` forces a text
    /// answer. Tools and `system_prompt` are still passed verbatim
    /// in either case so the prompt-cache prefix stays stable.
    pub tool_choice: tau_proto::ToolChoice,
    /// Hint from the harness for stateful chaining: the previous
    /// turn's `response_id` and the index in `context_items` where new
    /// content for this turn begins. Backends that don't support
    /// stateful chaining (Chat Completions) ignore this and replay
    /// the full item slice. The Responses backend slices
    /// `context_items[index..]` and sets `previous_response_id` on
    /// the upstream call.
    pub previous_response: Option<PreviousResponse<'a>>,
    /// Who originated this prompt — the interactive user, or an
    /// extension-side sub-agent query (most notably `core-delegate`).
    /// Folded into the wire `prompt_cache_key` so concurrent
    /// delegated turns don't share a routing bucket with the user's
    /// own turns (OpenAI's deployment checklist warns that >15 RPM
    /// per `(prefix, prompt_cache_key)` overflows to additional
    /// machines and degrades hit rate).
    pub originator: &'a PromptOriginator,
    /// When `true`, force the wire `prompt_cache_key` to the user's
    /// session-scoped base key for this turn even though
    /// [`Self::originator`] is an extension. Lets a single-shot side query
    /// (idle-summary) reuse the user's already-warm prefix cache. See
    /// [`mix_originator_into_cache_key`].
    pub share_user_cache_key: bool,
    /// Harness session this prompt belongs to. Used by the Responses
    /// WebSocket pool to key per-conversation connections — same
    /// session stays on the same socket across turns, so the
    /// connection-local `previous_response_id` cache stays warm.
    /// Backends without a connection pool ignore this.
    pub session_id: &'a SessionId,
}

/// See [`PromptPayload::previous_response`].
#[derive(Clone, Copy)]
pub struct PreviousResponse<'a> {
    pub id: &'a str,
    pub next_item_index: usize,
    pub transport: Option<AgentBackendTransport>,
}

/// Transport / protocol error returned from any LLM backend stream.
#[derive(Debug)]
pub enum LlmError {
    Http(Box<ureq::Error>),
    HttpStatus(u16, String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::HttpStatus(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
        }
    }
}

impl std::error::Error for LlmError {}

impl LlmError {
    /// Whether this error is plausibly transient and worth retrying.
    ///
    /// We treat transport hiccups, mid-stream IO breaks, and
    /// server-side stream errors (overload, upstream timeout) as
    /// retryable. JSON parse failures, missing-choices, and 4xx
    /// statuses other than 408/425/429 are treated as our bug or a
    /// deterministic request-level rejection — retrying just burns
    /// quota.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http(_) => Some(Duration::ZERO),
            Self::Io(_) => Some(Duration::ZERO),
            Self::Json(_) => None,
            Self::HttpStatus(code, body) => match *code {
                408 | 425 => Some(Duration::ZERO),
                429 => usage_limit_retry_after(body),
                500..=599 => Some(Duration::ZERO),
                // Code 0 is synthesized by the Responses backend for
                // SSE-level events: the body is prefixed with
                // "stream error:" (mid-stream provider hiccup —
                // overload, upstream timeout, gateway reset),
                // "response failed:" (deterministic model error),
                // or "response incomplete:" (request-level cap).
                // Only the first class is worth retrying — and even
                // then, account-level caps (usage_limit_reached,
                // rate_limit_exceeded, quota_exceeded) arrive
                // through this path as "stream error: …" and are
                // *not* transient. The error type is tagged in the
                // body suffix by `responses::apply_event`.
                0 if body.starts_with("stream error:") => {
                    if is_account_limit_body(body) {
                        None
                    } else {
                        Some(Duration::ZERO)
                    }
                }
                _ => None,
            },
        }
    }
}

/// Account-level limits that won't clear with any reasonable backoff —
/// matched against the `(type=…)` suffix that
/// `responses::apply_event` attaches to a `stream error: …` body. New
/// upstream error types can be added here as we encounter them;
/// false negatives just mean we burn a few retries before failing.
///
/// Exposed for the WS pool's `is_recoverable_ws_error` carve-out so
/// the pool doesn't reopen a socket just to hit the same cap on the
/// fresh one.
pub fn is_account_limit_body(body: &str) -> bool {
    body.contains("(type=usage_limit_reached)")
        || body.contains("(type=rate_limit_exceeded)")
        || body.contains("(type=quota_exceeded)")
        || body.contains("(type=billing_hard_limit_reached)")
        || body.contains("(type=insufficient_quota)")
}

fn usage_limit_retry_after(body: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if error.get("type")?.as_str()? != "usage_limit_reached" {
        return None;
    }
    if let Some(seconds) = error
        .get("resets_in_seconds")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(Duration::from_secs(seconds));
    }
    let resets_at = error.get("resets_at")?.as_u64()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(resets_at.saturating_sub(now)))
}

/// One provider output item as it is incrementally assembled from a
/// streaming response. This is intentionally item-shaped: final
/// `AgentResponseFinished.output_items` must be a projection of the
/// stream's item timeline, not a late re-bucketing of text/reasoning/tool
/// calls.
#[derive(Clone, Debug)]
pub enum OutputItemAccumulator {
    Empty,
    Message(MessageAccumulator),
    ToolCall(ToolCallAccumulator),
    Reasoning(OpaqueProviderItem),
}

/// Accumulates one assistant message item across text deltas.
#[derive(Clone, Debug, Default)]
pub struct MessageAccumulator {
    pub text: String,
    pub phase: Option<tau_proto::MessagePhase>,
}

/// Accumulated streaming state shared by both backends.
pub struct StreamState {
    /// Concatenated visible assistant text, kept for the existing
    /// `AgentResponseUpdated.text` wire shape. The durable final output
    /// is assembled from `output_items` instead.
    pub text: String,
    pub output_items: Vec<OutputItemAccumulator>,
    pub input_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Provider-supplied reasoning summary accumulated so far. `None`
    /// when the provider hasn't emitted any summary content (or when
    /// summaries weren't requested).
    pub thinking: Option<String>,
    /// Provider-supplied `response.id`, used by the harness to chain
    /// the next turn off this one via `previous_response_id`. Only
    /// populated by the Responses backend; the Chat Completions
    /// backend leaves this `None`.
    pub response_id: Option<String>,
    /// Opaque Responses-API input items returned by a standalone
    /// compaction call.
    pub compacted_input_items: Vec<String>,
    /// Raw terminal provider event for Responses streams (`response.completed`
    /// / `response.done`), retained for per-session debug captures. Other
    /// backends leave this empty.
    pub provider_terminal_event: Option<serde_json::Value>,
    /// A stale `previous_response_id` was rejected and this successful stream
    /// came from the full-replay retry.
    pub stale_chain_fallback: bool,
    /// Synthesized item slot for plain assistant text content.
    chat_message_item_index: Option<usize>,
}

/// Accumulates one tool call across streaming chunks.
#[derive(Clone, Debug)]
pub struct ToolCallAccumulator {
    pub id: String,
    pub name: String,
    pub tool_type: tau_proto::ToolType,
    pub arguments_json: String,
}

impl ToolCallAccumulator {
    pub fn new(tool_type: tau_proto::ToolType) -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            tool_type,
            arguments_json: String::new(),
        }
    }

    fn into_context_item(self) -> Option<ContextItem> {
        if self.name.is_empty() {
            return None;
        }
        let arguments = match self.tool_type {
            tau_proto::ToolType::Function => {
                let args: serde_json::Value =
                    serde_json::from_str(&self.arguments_json).unwrap_or(serde_json::Value::Null);
                json_to_cbor(&args)
            }
            tau_proto::ToolType::Custom => CborValue::Text(self.arguments_json),
        };
        let name = tau_proto::ToolName::try_new(self.name)?;
        Some(ContextItem::ToolCall(ToolCallItem {
            call_id: self.id.into(),
            name,
            tool_type: self.tool_type,
            arguments,
        }))
    }
}

impl Default for StreamState {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            output_items: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking: None,
            response_id: None,
            compacted_input_items: Vec::new(),
            provider_terminal_event: None,
            stale_chain_fallback: false,
            chat_message_item_index: None,
        }
    }

    fn ensure_output_len(&mut self, output_index: usize) {
        while self.output_items.len() <= output_index {
            self.output_items.push(OutputItemAccumulator::Empty);
        }
    }

    pub fn message_at_mut(&mut self, output_index: usize) -> &mut MessageAccumulator {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::Message(_)
        ) {
            self.output_items[output_index] =
                OutputItemAccumulator::Message(MessageAccumulator::default());
        }
        let OutputItemAccumulator::Message(message) = &mut self.output_items[output_index] else {
            unreachable!("message slot was just initialized");
        };
        message
    }

    pub fn append_message_delta_at(&mut self, output_index: usize, delta: &str) {
        self.message_at_mut(output_index).text.push_str(delta);
        self.refresh_text();
    }

    pub fn set_message_text_at(&mut self, output_index: usize, text: &str) {
        self.message_at_mut(output_index).text = text.to_owned();
        self.refresh_text();
    }

    pub fn set_message_phase_at(
        &mut self,
        output_index: usize,
        phase: Option<tau_proto::MessagePhase>,
    ) {
        if let Some(phase) = phase {
            self.message_at_mut(output_index).phase = Some(phase);
        }
    }

    pub fn append_chat_message_delta(&mut self, delta: &str) {
        let output_index = match self.chat_message_item_index {
            Some(output_index) => output_index,
            None => {
                let output_index = self.output_items.len();
                self.output_items
                    .push(OutputItemAccumulator::Message(MessageAccumulator::default()));
                self.chat_message_item_index = Some(output_index);
                output_index
            }
        };
        self.append_message_delta_at(output_index, delta);
    }

    pub fn tool_call_at_mut(
        &mut self,
        output_index: usize,
        tool_type: tau_proto::ToolType,
    ) -> &mut ToolCallAccumulator {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::ToolCall(_)
        ) {
            self.output_items[output_index] =
                OutputItemAccumulator::ToolCall(ToolCallAccumulator::new(tool_type));
        }
        let OutputItemAccumulator::ToolCall(call) = &mut self.output_items[output_index] else {
            unreachable!("tool-call slot was just initialized");
        };
        call.tool_type = tool_type;
        call
    }

    pub fn set_reasoning_item_json_at(&mut self, output_index: usize, item: &str) {
        if let Some(item) = opaque_item_from_json(item) {
            self.ensure_output_len(output_index);
            self.output_items[output_index] = OutputItemAccumulator::Reasoning(item);
        }
    }

    fn refresh_text(&mut self) {
        self.text.clear();
        for item in &self.output_items {
            if let OutputItemAccumulator::Message(message) = item {
                self.text.push_str(&message.text);
            }
        }
    }

    /// Returns the final assistant output items in provider item order.
    ///
    /// Tool-call accumulators with an empty `name` are dropped as stream
    /// artifacts. The streaming paths eagerly create slots from
    /// argument-delta events so the index stays addressable; if the
    /// matching name-carrying event never arrives, shipping it
    /// downstream would surface as an `invalid_tool` rejection in the
    /// harness even though the model never committed a valid call.
    pub fn into_output_items(self) -> Vec<ContextItem> {
        let mut items = Vec::new();

        for item in self.output_items {
            match item {
                OutputItemAccumulator::Empty => {}
                OutputItemAccumulator::Message(message) => {
                    if !message.text.is_empty() {
                        items.push(assistant_text_item_with_phase(message.text, message.phase));
                    }
                }
                OutputItemAccumulator::ToolCall(call) => {
                    if let Some(item) = call.into_context_item() {
                        items.push(item);
                    }
                }
                OutputItemAccumulator::Reasoning(item) => items.push(ContextItem::Reasoning(item)),
            }
        }

        if items.is_empty() && !self.text.is_empty() {
            items.push(assistant_text_item(self.text));
        }

        for item in self.compacted_input_items {
            if let Some(item) = opaque_item_from_json(&item) {
                items.push(ContextItem::Compaction(item));
            }
        }

        items
    }

    pub fn usage(&self) -> Option<AgentTokenUsage> {
        let input = self.input_tokens.unwrap_or(0);
        let cached = self.cached_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        if input == 0 && cached == 0 && output == 0 {
            None
        } else {
            Some(AgentTokenUsage {
                model: None,
                prompt_sent_tokens: input,
                prompt_cached_tokens: cached,
                response_received_tokens: output,
                stats: Default::default(),
            })
        }
    }
}

pub fn assistant_text_item(text: impl Into<String>) -> ContextItem {
    assistant_text_item_with_phase(text.into(), None)
}

pub fn assistant_text_item_with_phase(
    text: impl Into<String>,
    phase: Option<tau_proto::MessagePhase>,
) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase,
    })
}

fn opaque_item_from_json(item: &str) -> Option<OpaqueProviderItem> {
    let value: serde_json::Value = serde_json::from_str(item).ok()?;
    Some(OpaqueProviderItem(json_to_cbor(&value)))
}

/// Maps `Effort` to the wire string the OpenAI Responses /
/// Chat Completions APIs accept. `Off` returns `None` so the field is
/// omitted from the request entirely.
pub fn effort_wire(level: tau_proto::Effort) -> Option<&'static str> {
    use tau_proto::Effort::*;
    match level {
        Off => None,
        Minimal => Some("minimal"),
        Low => Some("low"),
        Medium => Some("medium"),
        High => Some("high"),
        XHigh => Some("xhigh"),
    }
}

/// Maps `Verbosity` to the wire string OpenAI's `verbosity` /
/// `text.verbosity` field accepts. There is no "off" sentinel — the
/// caller gates the field on a provider-level `supports_verbosity`
/// flag instead.
pub fn verbosity_wire(level: tau_proto::Verbosity) -> &'static str {
    level.as_openai_wire()
}

/// Derive the per-(provider endpoint, session) cache key the OpenAI
/// guide expects.
pub fn prompt_cache_key_for(base_url: &str, session_id: &SessionId) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(base_url.as_bytes());
    hasher.update(b"\0");
    hasher.update(session_id.as_str().as_bytes());
    format!("tau-{}", hasher.finalize().to_hex())
}

/// Produce the wire `prompt_cache_key` for an outgoing request from a
/// per-`(base_url, session_id)` base key, the originator of the current
/// prompt, and a `share_user_bucket` override.
///
/// User turns pass `base` through unchanged so a single interactive
/// session's successive turns keep landing on the same cache machine.
/// Extension-originated turns (e.g. `core-delegate` sub-agents) get a
/// distinct key derived from the extension's *name* so:
///   - sub-agent traffic doesn't pile onto the user's provider routing bucket —
///     OpenAI's deployment checklist warns that >15 RPM per `(prefix,
///     prompt_cache_key)` overflows to additional machines and degrades cache
///     effectiveness, and a parallel-delegate turn easily blows past that on a
///     shared key;
///   - the sub-agent's own multi-turn loop still reuses *its* cache because the
///     query id is intentionally NOT mixed in.
///
/// When `share_user_bucket` is `true`, the extension branch is skipped
/// and the session-scoped base key is returned. Used by the harness
/// for non-fan-out side queries (idle-summary) so a single side turn
/// can hit the user's already-warm prefix cache. Delegate sub-agents leave
/// it `false` to preserve the fan-out isolation above.
///
/// `None` in / `None` out: when the resolver chose not to send a
/// prompt cache key (provider doesn't support it), no key is sent
/// regardless of originator.
#[must_use]
pub fn mix_originator_into_cache_key(
    base: Option<&str>,
    originator: &PromptOriginator,
    share_user_bucket: bool,
) -> Option<String> {
    let base = base?;
    if share_user_bucket {
        return Some(base.to_owned());
    }
    match originator {
        PromptOriginator::User => Some(base.to_owned()),
        PromptOriginator::Extension { name, .. } => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(base.as_bytes());
            hasher.update(b"\0ext:");
            hasher.update(name.as_str().as_bytes());
            Some(format!("tau-{}", hasher.finalize().to_hex()))
        }
    }
}

// ---------------------------------------------------------------------------
// CBOR ↔ JSON value conversion
// ---------------------------------------------------------------------------

pub fn cbor_to_json(v: &CborValue) -> serde_json::Value {
    match v {
        CborValue::Null => serde_json::Value::Null,
        CborValue::Bool(b) => serde_json::Value::Bool(*b),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            serde_json::json!(n)
        }
        CborValue::Float(f) => serde_json::json!(f),
        CborValue::Text(s) => serde_json::Value::String(s.clone()),
        CborValue::Bytes(bytes) => serde_json::Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bytes,
        )),
        CborValue::Array(arr) => serde_json::Value::Array(arr.iter().map(cbor_to_json).collect()),
        CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                map.insert(key, cbor_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        CborValue::Tag(_, inner) => cbor_to_json(inner),
        other => {
            tracing::warn!(target: crate::LOG_TARGET, "unsupported CBOR value in tool input: {other:?}");
            serde_json::Value::Null
        }
    }
}

pub fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                CborValue::Integer(u.into())
            } else if let Some(f) = n.as_f64() {
                CborValue::Float(f)
            } else {
                CborValue::Null
            }
        }
        serde_json::Value::String(s) => CborValue::Text(s.clone()),
        serde_json::Value::Array(arr) => CborValue::Array(arr.iter().map(json_to_cbor).collect()),
        serde_json::Value::Object(map) => CborValue::Map(
            map.iter()
                .map(|(k, v)| (CborValue::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests;
