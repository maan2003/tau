//! First-party agent process.
//!
//! Receives `SessionPromptCreated` / `SessionCompactionRequested`
//! from the harness and emits `AgentResponseUpdated` /
//! `AgentResponseFinished` events.

pub mod common;
pub(crate) mod openai;
mod responses;

use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use backon::BackoffBuilder;
use tau_config::settings;
use tau_proto::{
    Ack, AgentPromptSubmitted, AgentResponseFinished, AgentResponseUpdated, ClientKind, Event,
    EventName, EventSelector, Frame, FrameReader, FrameWriter, Hello, Message, PROTOCOL_VERSION,
    Ready, Subscribe,
};

/// `tracing` target for events emitted from the agent. Matches the
/// convention described in [`tau_extension`]: a short identifier the
/// user can name in `TAU_LOG=agent=trace` to dump every prompt
/// the harness hands the agent.
pub const LOG_TARGET: &str = "agent";

fn materialize_prompt(
    prompt: &tau_proto::SessionPromptCreated,
    snapshots: &HashMap<tau_proto::SessionPromptId, tau_proto::SessionPromptCreated>,
) -> Result<tau_proto::SessionPromptCreated, String> {
    let mut materialized = prompt.clone();
    if let Some(system_prompt_ref) = &prompt.system_prompt_ref {
        let base = snapshots
            .get(&system_prompt_ref.base_session_prompt_id)
            .ok_or_else(|| {
                format!(
                    "missing prompt system prompt base {}",
                    system_prompt_ref.base_session_prompt_id
                )
            })?;
        materialized.system_prompt = base.system_prompt.clone();
        materialized.system_prompt_ref = None;
    }
    if let Some(prefix) = &prompt.message_prefix {
        let base = snapshots
            .get(&prefix.base_session_prompt_id)
            .ok_or_else(|| {
                format!(
                    "missing prompt prefix base {}",
                    prefix.base_session_prompt_id
                )
            })?;
        if base.messages.len() < prefix.message_count {
            return Err(format!(
                "prompt prefix base {} has only {} messages, need {}",
                prefix.base_session_prompt_id,
                base.messages.len(),
                prefix.message_count
            ));
        }
        let mut messages = base.messages[..prefix.message_count].to_vec();
        messages.extend(prompt.messages.clone());
        materialized.messages = messages;
        materialized.message_prefix = None;
    }
    if let Some(tools_ref) = &prompt.tools_ref {
        let base = snapshots
            .get(&tools_ref.base_session_prompt_id)
            .ok_or_else(|| {
                format!(
                    "missing prompt tools base {}",
                    tools_ref.base_session_prompt_id
                )
            })?;
        materialized.tools = base.tools.clone();
        materialized.tools_ref = None;
    }
    Ok(materialized)
}

/// Runs the agent on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging();
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the agent over arbitrary reader/writer streams.
///
/// The reader is moved to a background thread so the main loop can
/// `recv_timeout` on it during retry-backoff sleeps and wake early
/// when the harness disconnects (or queues another event).
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    let model_registry = settings::load_models().unwrap_or_default();

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent".into(),
        client_kind: ClientKind::Agent,
    })))?;
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SESSION_COMPACTION_REQUESTED),
            EventSelector::Exact(EventName::SESSION_PROMPT_CREATED),
            EventSelector::Exact(EventName::SESSION_PROMPT_PREWARM_REQUESTED),
            EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
        ],
    })))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("agent ready".to_owned()),
    })))?;
    writer.flush()?;

    // Pump frames from the reader into a channel. The main loop consumes
    // from `frame_rx`; backoff sleeps use `recv_timeout` on the same
    // receiver so a `Disconnect` message (or sender drop on EOF) wakes
    // us out of a wait we'd otherwise be deaf to.
    let (frame_tx, frame_rx) = mpsc::channel::<Frame>();
    thread::spawn(move || {
        let mut reader = FrameReader::new(BufReader::new(reader));
        loop {
            match reader.read_frame() {
                Ok(Some(frame)) => {
                    if frame_tx.send(frame).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(target: LOG_TARGET, "reader pump failed: {error}");
                    return;
                }
            }
        }
    });

    let mut deferred: VecDeque<Frame> = VecDeque::new();
    // Per-process WS state, lifted out of the per-prompt scope so
    // connections survive between turns. See `TODO-codex-websocket.md`
    // §2 for the design.
    let mut ws_pool = responses::pool::WsPool::new();
    // Sessions where the WS upgrade or stream got slapped down with
    // an unambiguous "go away" — flip them permanently to HTTP for
    // the rest of this agent's life, rather than retrying WS on
    // every turn.
    let mut ws_disabled: HashSet<String> = HashSet::new();
    // Spids the harness asked us to cancel via a targeted
    // `UiCancelPrompt` (`session_prompt_id: Some(...)`). Populated
    // from two places: (a) the main loop below, when a cancel is
    // received before the corresponding prompt/compaction request has
    // been dispatched, and (b) `RetryContext::sleep_or_abort`, when
    // a cancel for a *different* spid arrives mid-retry on the
    // currently in-flight one. Each prompt-shaped request is
    // checked against this set before being handed to
    // `handle_prompt` / `handle_compaction_request`, and dropped with a stub
    // finished event if present. This is the gate that prevents the
    // `preempt_blocking_ext_side_conversations` cancel from running
    // a still-queued side conv that the harness has already given
    // up on.
    let mut canceled_spids: HashSet<tau_proto::SessionPromptId> = HashSet::new();
    let mut prompt_snapshots: HashMap<tau_proto::SessionPromptId, tau_proto::SessionPromptCreated> =
        HashMap::new();

    loop {
        let frame = match deferred.pop_front() {
            Some(e) => e,
            None => match frame_rx.recv() {
                Ok(e) => e,
                Err(_) => return Ok(()),
            },
        };
        // Peel the LogEvent envelope. The agent processes one prompt at
        // a time (serial), so acks are trivially in order: ack right
        // after handling whatever is inside.
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::SessionPromptPrewarmRequested(prewarm)) => {
                handle_prewarm(&prewarm, &model_registry, &mut ws_pool, &mut ws_disabled);
            }
            Frame::Event(Event::SessionCompactionRequested(request)) => {
                let session_prompt_id = request.prompt.session_prompt_id.clone();

                let prompt = match materialize_prompt(&request.prompt, &prompt_snapshots) {
                    Ok(prompt) => prompt,
                    Err(error) => {
                        writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id: session_prompt_id.clone(),
                                text: Some(error),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                output_tokens: None,
                                thinking: None,
                                token_usage: None,
                                originator: request.prompt.originator.clone(),
                                backend: None,
                                response_id: None,
                                phase: None,
                                reasoning_items: Vec::new(),
                                compacted_input_items: Vec::new(),
                                ws_pool_delta: None,
                            },
                        )))?;
                        writer.flush()?;
                        if let Some(id) = log_id {
                            writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                            writer.flush()?;
                        }
                        continue;
                    }
                };

                if canceled_spids.remove(&session_prompt_id) {
                    tracing::info!(
                        target: LOG_TARGET,
                        session_prompt_id = %session_prompt_id,
                        "skipping compaction request — already canceled by harness",
                    );
                    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                        AgentResponseFinished {
                            session_prompt_id: session_prompt_id.clone(),
                            text: Some("(cancelled by harness)".to_owned()),
                            tool_calls: Vec::new(),
                            input_tokens: None,
                            cached_tokens: None,
                            output_tokens: None,
                            thinking: None,
                            token_usage: None,
                            originator: prompt.originator.clone(),
                            backend: None,
                            response_id: None,
                            phase: None,
                            reasoning_items: Vec::new(),
                            compacted_input_items: Vec::new(),
                            ws_pool_delta: None,
                        },
                    )))?;
                    writer.flush()?;
                    if let Some(id) = log_id {
                        writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                        writer.flush()?;
                    }
                    continue;
                }

                if tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
                    match serde_json::to_string_pretty(&request) {
                        Ok(json) => tracing::trace!(
                            target: LOG_TARGET,
                            session_prompt_id = %session_prompt_id,
                            "agent compaction request:\n{json}"
                        ),
                        Err(error) => tracing::trace!(
                            target: LOG_TARGET,
                            session_prompt_id = %session_prompt_id,
                            "agent compaction request (failed to serialize for log: {error})"
                        ),
                    }
                }

                writer.write_frame(&Frame::Event(Event::AgentPromptSubmitted(
                    AgentPromptSubmitted {
                        session_prompt_id: session_prompt_id.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;
                writer.flush()?;

                let mut retry_ctx = RetryContext {
                    frame_rx: &frame_rx,
                    deferred: &mut deferred,
                    canceled_spids: &mut canceled_spids,
                };

                let mut auth_store = tau_provider::storage::load().unwrap_or_default();
                let backend = prompt
                    .model
                    .as_ref()
                    .and_then(|m| tau_provider::resolve(m, &model_registry, &mut auth_store))
                    .map(BackendConfig::from);

                match backend {
                    Some(backend) => {
                        handle_compaction_request(
                            &session_prompt_id,
                            &backend,
                            &prompt,
                            &mut writer,
                            &mut retry_ctx,
                        )?;
                    }
                    None => {
                        let msg = match &prompt.model {
                            Some(m) => format!("cannot resolve model config for: {m}"),
                            None => "no model specified".to_owned(),
                        };
                        writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id,
                                text: Some(msg),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                output_tokens: None,
                                thinking: None,
                                token_usage: None,
                                originator: prompt.originator.clone(),
                                backend: None,
                                response_id: None,
                                phase: None,
                                reasoning_items: Vec::new(),
                                compacted_input_items: Vec::new(),
                                ws_pool_delta: None,
                            },
                        )))?;
                        writer.flush()?;
                    }
                }
            }
            Frame::Event(Event::SessionPromptCreated(prompt)) => {
                let session_prompt_id = prompt.session_prompt_id.clone();

                let prompt = match materialize_prompt(&prompt, &prompt_snapshots) {
                    Ok(prompt) => prompt,
                    Err(error) => {
                        writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id: session_prompt_id.clone(),
                                text: Some(error),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                output_tokens: None,
                                thinking: None,
                                token_usage: None,
                                originator: prompt.originator.clone(),
                                backend: None,
                                response_id: None,
                                phase: None,
                                reasoning_items: Vec::new(),
                                compacted_input_items: Vec::new(),
                                ws_pool_delta: None,
                            },
                        )))?;
                        writer.flush()?;
                        if let Some(id) = log_id {
                            writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                            writer.flush()?;
                        }
                        continue;
                    }
                };
                prompt_snapshots.insert(session_prompt_id.clone(), prompt.clone());

                // Drop a prompt the harness asked us to cancel before
                // we could even dequeue it. Two ways this triggers:
                //   - A `preempt_blocking_ext_side_conversations` cancel arrived while we were
                //     retry-sleeping on a different spid; `sleep_or_abort` parked the cancel in
                //     this set instead of mis-aborting the wrong attempt.
                //   - A targeted cancel raced ahead of its `SessionPromptCreated` (unusual but
                //     possible if the harness publishes both inside the same tick).
                // Either way, we surface a stub finished event so the
                // harness's prompt-routing book-keeping closes cleanly
                // and the spid isn't left dangling.
                if canceled_spids.remove(&session_prompt_id) {
                    tracing::info!(
                        target: LOG_TARGET,
                        session_prompt_id = %session_prompt_id,
                        "skipping prompt — already canceled by harness",
                    );
                    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                        AgentResponseFinished {
                            session_prompt_id: session_prompt_id.clone(),
                            text: Some("(cancelled by harness)".to_owned()),
                            tool_calls: Vec::new(),
                            input_tokens: None,
                            cached_tokens: None,
                            output_tokens: None,
                            thinking: None,
                            token_usage: None,
                            originator: prompt.originator.clone(),
                            backend: None,
                            response_id: None,
                            phase: None,
                            reasoning_items: Vec::new(),
                            compacted_input_items: Vec::new(),
                            ws_pool_delta: None,
                        },
                    )))?;
                    writer.flush()?;
                    if let Some(id) = log_id {
                        writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                        writer.flush()?;
                    }
                    continue;
                }

                // Full prompt dump for debugging. Off by default;
                // enable with `TAU_LOG=agent=trace`. Pretty JSON
                // is the most readable form for a multi-screen
                // payload, and the prompt is already fully
                // serializable.
                if tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
                    match serde_json::to_string_pretty(&prompt) {
                        Ok(json) => tracing::trace!(
                            target: LOG_TARGET,
                            session_prompt_id = %session_prompt_id,
                            "agent prompt:\n{json}"
                        ),
                        Err(error) => tracing::trace!(
                            target: LOG_TARGET,
                            session_prompt_id = %session_prompt_id,
                            "agent prompt (failed to serialize for log: {error})"
                        ),
                    }
                }

                // Announce we accepted the prompt.
                writer.write_frame(&Frame::Event(Event::AgentPromptSubmitted(
                    AgentPromptSubmitted {
                        session_prompt_id: session_prompt_id.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;
                writer.flush()?;

                let mut retry_ctx = RetryContext {
                    frame_rx: &frame_rx,
                    deferred: &mut deferred,
                    canceled_spids: &mut canceled_spids,
                };

                // Resolve backend from the model specified in the prompt.
                // Reload auth on every prompt so `tau provider login` or
                // `/provider-auth` takes effect without restarting Tau.
                let mut auth_store = tau_provider::storage::load().unwrap_or_default();
                let backend = prompt
                    .model
                    .as_ref()
                    .and_then(|m| tau_provider::resolve(m, &model_registry, &mut auth_store))
                    .map(BackendConfig::from);

                match backend {
                    Some(backend) => {
                        handle_prompt(
                            &session_prompt_id,
                            &backend,
                            &prompt,
                            &mut writer,
                            &mut retry_ctx,
                            &mut ws_pool,
                            &mut ws_disabled,
                        )?;
                    }
                    None => {
                        let msg = match &prompt.model {
                            Some(m) => format!("cannot resolve model config for: {m}"),
                            None => "no model specified".to_owned(),
                        };
                        writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id,
                                text: Some(msg),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                output_tokens: None,
                                thinking: None,
                                token_usage: None,
                                originator: prompt.originator.clone(),
                                // No backend ran: model failed to resolve.
                                backend: None,
                                response_id: None,
                                phase: None,
                                reasoning_items: Vec::new(),
                                compacted_input_items: Vec::new(),
                                ws_pool_delta: None,
                            },
                        )))?;
                        writer.flush()?;
                    }
                }
            }
            Frame::Event(Event::UiCancelPrompt(cancel)) => {
                // Targeted cancel that arrived while the agent was
                // idle (no retry sleep in progress) — typically a
                // `preempt_blocking_ext_side_conversations` cancel
                // for a side conv whose `SessionPromptCreated` is
                // still queued behind it in `frame_rx`. Record the
                // spid so the gate above drops the prompt when it
                // pops. Broadcast cancels (`None`) are no-ops here:
                // there's nothing in flight to abort.
                if let Some(spid) = cancel.session_prompt_id {
                    canceled_spids.insert(spid);
                }
            }
            Frame::Message(Message::Disconnect(_)) => return Ok(()),
            _ => {}
        }
        if let Some(id) = log_id {
            writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
            writer.flush()?;
        }
    }
}

/// What the retry loop needs from the agent's main event pump:
/// access to the channel of incoming events (so a long backoff sleep
/// can wake on disconnect / queued prompts), a deferred buffer for
/// events that arrive mid-sleep but belong to a later main-loop
/// iteration, and the shared `canceled_spids` set so a targeted cancel
/// arriving for a *different* in-flight spid can be recorded for the
/// main loop's gate instead of mis-aborting the current attempt.
struct RetryContext<'a> {
    frame_rx: &'a Receiver<Frame>,
    deferred: &'a mut VecDeque<Frame>,
    canceled_spids: &'a mut HashSet<tau_proto::SessionPromptId>,
}

/// Outcome of an interruptible sleep.
enum SleepOutcome {
    /// Slept the full duration.
    Elapsed,
    /// Woken by a `LifecycleDisconnect` (or reader EOF). The caller
    /// should stop retrying and finalize the prompt.
    Aborted,
}

impl<'a> RetryContext<'a> {
    /// Sleep for up to `delay`, but wake early if the harness sends a
    /// `LifecycleDisconnect` or a `UiCancelPrompt` targeting the
    /// prompt this retry loop is owning. Cancels for a different spid
    /// (e.g. a side-conv preempt that arrived while we're retrying a
    /// user/delegate prompt) are buffered for the main loop instead
    /// of aborting the wrong attempt. A targetless cancel
    /// (`session_prompt_id: None` — the legacy `/cancel`) still
    /// aborts whatever's in flight.
    fn sleep_or_abort(&mut self, delay: Duration, current_spid: &str) -> SleepOutcome {
        let deadline = Instant::now() + delay;
        loop {
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return SleepOutcome::Elapsed;
            };
            match self.frame_rx.recv_timeout(remaining) {
                Err(RecvTimeoutError::Timeout) => return SleepOutcome::Elapsed,
                Err(RecvTimeoutError::Disconnected) => return SleepOutcome::Aborted,
                Ok(frame) => {
                    if let Frame::Event(Event::UiCancelPrompt(cancel)) = &frame {
                        match &cancel.session_prompt_id {
                            // Broadcast cancel (legacy `/cancel`):
                            // abort whatever's in flight.
                            None => {
                                self.deferred.push_back(frame);
                                return SleepOutcome::Aborted;
                            }
                            Some(spid) if spid.as_str() == current_spid => {
                                self.deferred.push_back(frame);
                                return SleepOutcome::Aborted;
                            }
                            Some(spid) => {
                                // Targeted at a *different* queued spid
                                // (e.g. side-conv preempt while we're
                                // retrying a user/delegate prompt).
                                // Don't push to deferred — the main
                                // loop has no use for the bare cancel
                                // event; just record the spid so its
                                // queued `SessionPromptCreated` is
                                // dropped when it pops.
                                self.canceled_spids.insert(spid.clone());
                                continue;
                            }
                        }
                    }
                    let abort = matches!(&frame, Frame::Message(Message::Disconnect(_)));
                    self.deferred.push_back(frame);
                    if abort {
                        return SleepOutcome::Aborted;
                    }
                    // Otherwise keep waiting out the rest of the backoff.
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Backend config resolution
// ---------------------------------------------------------------------------

enum BackendConfig {
    ChatCompletions(openai::OpenAiConfig),
    Responses(responses::ResponsesConfig),
}

impl BackendConfig {
    /// Dispatch a streaming call to the appropriate HTTP backend.
    ///
    /// Pure HTTP+SSE — the WebSocket dispatch lives in
    /// [`stream_with_ws`] because it needs the agent loop's
    /// connection pool and per-session fallback state, which a
    /// per-prompt `&BackendConfig` cannot reach.
    fn stream_http(
        &self,
        request: &common::PromptPayload<'_>,
        on_update: &mut impl FnMut(&str, Option<&str>),
    ) -> Result<common::StreamState, common::LlmError> {
        match self {
            Self::ChatCompletions(cfg) => openai::chat_completion_stream(cfg, request, on_update),
            Self::Responses(cfg) => responses::responses_stream(cfg, request, on_update),
        }
    }

    /// Wire-form descriptor stamped on the turn's
    /// `AgentResponseFinished` so offline inspection knows which
    /// backend produced it without each request having to log
    /// separately. `transport` is the actual wire path the turn took
    /// — captured at dispatch time by [`stream_with_dispatch`].
    fn descriptor(&self, transport: tau_proto::AgentBackendTransport) -> tau_proto::AgentBackend {
        match self {
            Self::ChatCompletions(cfg) => tau_proto::AgentBackend {
                kind: tau_proto::AgentBackendKind::ChatCompletions,
                base_url: cfg.base_url.clone(),
                transport,
            },
            Self::Responses(cfg) => tau_proto::AgentBackend {
                kind: tau_proto::AgentBackendKind::Responses,
                base_url: cfg.base_url.clone(),
                transport,
            },
        }
    }
}

impl From<tau_provider::resolver::ResolvedBackend> for BackendConfig {
    fn from(value: tau_provider::resolver::ResolvedBackend) -> Self {
        match value {
            tau_provider::resolver::ResolvedBackend::ChatCompletions(cfg) => {
                Self::ChatCompletions(openai::OpenAiConfig {
                    base_url: cfg.base_url,
                    api_key: cfg.api_key,
                    model_id: cfg.model_id.into_string(),
                    supports_reasoning_effort: cfg.supports_reasoning_effort,
                    supports_verbosity: cfg.supports_verbosity,
                    supports_prompt_cache_key: cfg.supports_prompt_cache_key,
                    prompt_cache_retention: cfg.prompt_cache_retention,
                    supports_llama_cpp_cache: cfg.supports_llama_cpp_cache,
                })
            }
            tau_provider::resolver::ResolvedBackend::Responses(cfg) => {
                Self::Responses(responses::ResponsesConfig {
                    base_url: cfg.base_url,
                    api_key: cfg.api_key,
                    model_id: cfg.model_id.into_string(),
                    account_id: cfg.account_id,
                    supports_reasoning_effort: cfg.supports_reasoning_effort,
                    supports_reasoning_summary: cfg.supports_reasoning_summary,
                    supports_verbosity: cfg.supports_verbosity,
                    supports_phase: cfg.supports_phase,
                    supports_encrypted_reasoning: cfg.supports_encrypted_reasoning,
                    supports_websocket: cfg.supports_websocket,
                    supports_compaction: cfg.supports_compaction,
                    supports_prompt_cache_key: cfg.supports_prompt_cache_key,
                    prompt_cache_retention: cfg.prompt_cache_retention,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LLM backends
// ---------------------------------------------------------------------------

/// Maximum number of retry attempts before giving up on a transient
/// LLM error. Combined with [`llm_retry_schedule`]'s fibonacci shape
/// (min 10s), this caps total wait time at roughly 9 minutes.
const LLM_MAX_RETRIES: usize = 8;

/// Tighter cap for extension-originated turns (delegate sub-agents,
/// `std-notifications`' idle-summary, etc.). These are best-effort
/// from the user's perspective — burning a 9-minute retry budget on
/// one of them blocks the agent's single-threaded prompt slot and
/// stalls the next user prompt. Pegged at 2 retries (~30 s total)
/// so a genuine transient hiccup still gets a fair shake.
const LLM_MAX_RETRIES_EXTENSION: usize = 2;

fn max_retries_for(originator: &tau_proto::PromptOriginator) -> usize {
    match originator {
        tau_proto::PromptOriginator::User => LLM_MAX_RETRIES,
        tau_proto::PromptOriginator::Extension { .. } => LLM_MAX_RETRIES_EXTENSION,
    }
}

/// Build a fibonacci backoff schedule for retrying transient LLM
/// errors. Roughly: 10s, 10s, 20s, 30s, 50s, 80s, 130s, 210s — eight
/// retries, ~9 minutes total wait before we give up. Jittered to
/// avoid lockstep retries from many agents hitting a recovering
/// upstream simultaneously.
fn llm_retry_schedule(max_attempts: usize) -> backon::FibonacciBackoff {
    backon::FibonacciBuilder::default()
        .with_min_delay(Duration::from_secs(10))
        .with_max_times(max_attempts)
        .with_jitter()
        .build()
}

/// Retry an LLM streaming call with fibonacci backoff for transient
/// errors.
///
/// `call` is re-invoked from scratch each attempt — if a previous
/// attempt streamed partial text via its `on_update`, the next
/// attempt's updates will overwrite it on the way through. Between
/// attempts we paint a transient retry banner over the response
/// block (via `AgentResponseUpdated.text`) so the user sees what's
/// happening; the banner is replaced by real content as soon as the
/// next attempt's first delta arrives.
///
/// The backoff sleep is interruptible: a `LifecycleDisconnect`
/// arriving mid-sleep aborts the retry and the call returns the
/// last error.
fn with_llm_retry<F, W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
    mut call: F,
) -> Result<common::StreamState, common::LlmError>
where
    F: FnMut(&mut FrameWriter<BufWriter<W>>) -> Result<common::StreamState, common::LlmError>,
{
    let max_attempts = max_retries_for(originator);
    let mut backoff = llm_retry_schedule(max_attempts);
    let mut attempt = 0_usize;
    loop {
        let error = match call(writer) {
            Ok(state) => return Ok(state),
            Err(e) => e,
        };
        let Some(retry_after) = error.retry_after() else {
            return Err(error);
        };
        let Some(backoff_delay) = backoff.next() else {
            return Err(error);
        };
        let delay = retry_after.max(backoff_delay);
        attempt += 1;
        tracing::warn!(
            target: LOG_TARGET,
            session_prompt_id = %session_prompt_id,
            "LLM error, retrying in {delay:?} (attempt {attempt}/{max_attempts}): {error}",
        );
        emit_retry_banner(
            session_prompt_id,
            originator,
            writer,
            &error,
            delay,
            attempt,
            max_attempts,
        );
        if matches!(
            retry_ctx.sleep_or_abort(delay, session_prompt_id),
            SleepOutcome::Aborted,
        ) {
            tracing::info!(
                target: LOG_TARGET,
                session_prompt_id = %session_prompt_id,
                "retry aborted by disconnect/cancel",
            );
            return Err(error);
        }
    }
}

/// Paint a transient banner into the assistant response block so the
/// user can see we're waiting on an upstream retry. Best-effort:
/// write/flush failures are dropped, matching how the streaming
/// `on_update` handles them.
fn emit_retry_banner<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut FrameWriter<BufWriter<W>>,
    error: &common::LlmError,
    delay: Duration,
    attempt: usize,
    max_attempts: usize,
) {
    let banner = format!(
        "provider error — retrying in {}s (attempt {}/{})\n\n> {}",
        delay.as_secs(),
        attempt,
        max_attempts,
        error,
    );
    let _ = writer.write_frame(&Frame::Event(Event::AgentResponseUpdated(
        AgentResponseUpdated {
            session_prompt_id: session_prompt_id.into(),
            text: banner,
            thinking: None,
            originator: originator.clone(),
        },
    )));
    let _ = writer.flush();
}

/// Single-attempt streaming dispatch used inside the retry loop.
///
/// For Chat Completions and HTTP-only Responses turns, this is just
/// [`BackendConfig::stream_http`]. For Responses turns with WS
/// enabled (and the per-session sticky-disable flag still off), it
/// tries the WS pool first; on an upgrade-failure-style error (HTTP
/// 426 or the sticky-disable WS-close cases the WS guide warns
/// about), it sets `ws_disabled` for this session and falls through
/// to HTTP for the rest of the agent's lifetime. Other errors
/// surface to the outer retry loop, which decides whether they're
/// retryable (`stream error: ...`) or terminal.
fn stream_with_dispatch(
    backend: &BackendConfig,
    request: &common::PromptPayload<'_>,
    ws_pool: &mut responses::pool::WsPool,
    ws_disabled: &mut HashSet<String>,
    transport_taken: &mut tau_proto::AgentBackendTransport,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<common::StreamState, common::LlmError> {
    if let BackendConfig::Responses(cfg) = backend {
        let session_id = request.session_id.as_str();
        let try_ws = cfg.supports_websocket && !ws_disabled.contains(session_id);
        if try_ws {
            match responses::pool::run_turn_through_pool(
                ws_pool, cfg, session_id, request, on_update,
            ) {
                Ok(state) => {
                    *transport_taken = tau_proto::AgentBackendTransport::Websocket;
                    return Ok(state);
                }
                Err(responses::pool::WsTurnError::HttpFallbackRecommended(error)) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        session_id,
                        "WS chain unavailable ({error}); falling back to HTTP for this session",
                    );
                    ws_disabled.insert(session_id.to_owned());
                    *transport_taken = tau_proto::AgentBackendTransport::HttpSse;
                    return backend.stream_http(request, on_update);
                }
                Err(error) if should_disable_ws_error(&error) => {
                    let error = error.into_llm_error();
                    tracing::warn!(
                        target: LOG_TARGET,
                        session_id,
                        "WS path failed ({error}); falling back to HTTP for this session",
                    );
                    ws_disabled.insert(session_id.to_owned());
                    // Fall through to the HTTP path below.
                }
                Err(other) => return Err(other.into_llm_error()),
            }
        }
    }
    *transport_taken = tau_proto::AgentBackendTransport::HttpSse;
    backend.stream_http(request, on_update)
}

/// True for WS-side failures that should trigger sticky fallback to
/// HTTP+SSE rather than just being retried on WS:
///
/// - **HTTP 426 Upgrade Required** during the WS handshake — the server told us
///   WS isn't available for this client right now.
/// - **`websocket_connection_limit_reached`** — codex CLI treats this as
///   retryable on WS, but flipping to HTTP after one bounce matches the
///   documented policy ("Don't loop on a hostile server" in
///   `TODO-codex-websocket.md` §3) and avoids burning the next prompt on
///   another doomed upgrade.
fn should_disable_ws_error(error: &responses::pool::WsTurnError) -> bool {
    match error {
        responses::pool::WsTurnError::HttpFallbackRecommended(_) => false,
        responses::pool::WsTurnError::Other(error) => should_disable_ws(error),
    }
}

fn should_disable_ws(error: &common::LlmError) -> bool {
    match error {
        common::LlmError::HttpStatus(426, _) => true,
        common::LlmError::HttpStatus(_, body) => {
            body.contains("websocket_connection_limit_reached")
        }
        _ => false,
    }
}

fn handle_prewarm(
    prewarm: &tau_proto::SessionPromptPrewarmRequested,
    model_registry: &settings::ModelRegistry,
    ws_pool: &mut responses::pool::WsPool,
    ws_disabled: &mut HashSet<String>,
) {
    let Some(model) = prewarm.model.as_ref() else {
        tracing::debug!(
            target: LOG_TARGET,
            session_id = %prewarm.session_id,
            "skipping prompt prewarm: no selected model",
        );
        return;
    };

    let mut auth_store = tau_provider::storage::load().unwrap_or_default();
    let Some(BackendConfig::Responses(cfg)) =
        tau_provider::resolve(model, model_registry, &mut auth_store).map(BackendConfig::from)
    else {
        tracing::debug!(
            target: LOG_TARGET,
            session_id = %prewarm.session_id,
            model = %model,
            "skipping prompt prewarm: unsupported backend",
        );
        return;
    };
    let session_id = prewarm.session_id.as_str();
    if !cfg.supports_websocket || ws_disabled.contains(session_id) {
        tracing::debug!(
            target: LOG_TARGET,
            session_id,
            "skipping prompt prewarm: websocket prewarm unsupported",
        );
        return;
    }

    let request = common::PromptPayload {
        system_prompt: &prewarm.system_prompt,
        messages: &prewarm.messages,
        compacted_input_items: &[],
        tools: &prewarm.tools,
        params: prewarm.model_params,
        tool_choice: prewarm.tool_choice,
        previous_response: None,
        originator: &prewarm.originator,
        share_user_cache_key: prewarm.share_user_cache_key,
        session_id: &prewarm.session_id,
    };
    tracing::debug!(target: LOG_TARGET, session_id, "starting prompt prewarm");
    match responses::pool::run_prewarm_through_pool(ws_pool, &cfg, session_id, &request) {
        Ok(_) => tracing::debug!(target: LOG_TARGET, session_id, "completed prompt prewarm"),
        Err(error) if should_disable_ws(&error) => {
            tracing::debug!(
                target: LOG_TARGET,
                session_id,
                "prompt prewarm disabled WS path: {error}",
            );
            ws_disabled.insert(session_id.to_owned());
        }
        Err(error) => tracing::debug!(
            target: LOG_TARGET,
            session_id,
            "prompt prewarm failed: {error}",
        ),
    }
}

fn handle_prompt<W: Write>(
    session_prompt_id: &str,
    backend: &BackendConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
    ws_pool: &mut responses::pool::WsPool,
    ws_disabled: &mut HashSet<String>,
) -> Result<(), Box<dyn Error>> {
    let request = common::PromptPayload {
        system_prompt: &prompt.system_prompt,
        messages: &prompt.messages,
        compacted_input_items: &prompt.compacted_input_items,
        tools: &prompt.tools,
        params: prompt.model_params,
        tool_choice: prompt.tool_choice,
        previous_response: prompt
            .previous_response
            .as_ref()
            .map(|p| common::PreviousResponse {
                id: p.id.as_str(),
                message_index: p.message_index,
            }),
        originator: &prompt.originator,
        share_user_cache_key: prompt.share_user_cache_key,
        session_id: &prompt.session_id,
    };

    let originator = prompt.originator.clone();
    // Captures which wire transport the *final* attempt actually
    // took. Each retry overwrites it — the descriptor stamped on the
    // emitted `AgentResponseFinished` therefore reflects the
    // surviving path (the WS attempt that succeeded, or the HTTP
    // fallback if WS bounced).
    let mut transport_taken = tau_proto::AgentBackendTransport::HttpSse;
    // Snapshot the WS pool counters before the turn so we can emit a
    // per-turn delta on `AgentResponseFinished`. Skipped entirely for
    // non-Responses backends (Chat Completions never touches the
    // pool) so an offline reader can tell "pool wasn't applicable"
    // from "pool was applicable but no event happened."
    let ws_pool_before = matches!(backend, BackendConfig::Responses(_)).then(|| ws_pool.stats());
    let result = with_llm_retry(
        session_prompt_id,
        &originator,
        writer,
        retry_ctx,
        |writer| {
            let mut on_update = |text_so_far: &str, thinking_so_far: Option<&str>| {
                let _ = writer.write_frame(&Frame::Event(Event::AgentResponseUpdated(
                    AgentResponseUpdated {
                        session_prompt_id: session_prompt_id.into(),
                        text: text_so_far.to_owned(),
                        thinking: thinking_so_far.map(str::to_owned),
                        originator: originator.clone(),
                    },
                )));
                let _ = writer.flush();
            };
            stream_with_dispatch(
                backend,
                &request,
                ws_pool,
                ws_disabled,
                &mut transport_taken,
                &mut on_update,
            )
        },
    );
    let backend_descriptor = backend.descriptor(transport_taken);
    let ws_pool_delta = ws_pool_before.map(|before| compute_ws_pool_delta(before, ws_pool.stats()));
    match result {
        Ok(state) => finish_stream(
            session_prompt_id,
            &prompt.originator,
            &backend_descriptor,
            state,
            ws_pool_delta,
            writer,
        )?,
        Err(error) => finish_error(
            session_prompt_id,
            &prompt.originator,
            &backend_descriptor,
            error,
            ws_pool_delta,
            writer,
        )?,
    }
    Ok(())
}

fn handle_compaction_request<W: Write>(
    session_prompt_id: &str,
    backend: &BackendConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
) -> Result<(), Box<dyn Error>> {
    let request = common::PromptPayload {
        system_prompt: &prompt.system_prompt,
        messages: &prompt.messages,
        compacted_input_items: &prompt.compacted_input_items,
        tools: &prompt.tools,
        params: prompt.model_params,
        tool_choice: prompt.tool_choice,
        previous_response: None,
        originator: &prompt.originator,
        share_user_cache_key: prompt.share_user_cache_key,
        session_id: &prompt.session_id,
    };
    let backend_descriptor = backend.descriptor(tau_proto::AgentBackendTransport::HttpSse);
    let result = match backend {
        BackendConfig::Responses(cfg) if cfg.supports_compaction => with_llm_retry(
            session_prompt_id,
            &prompt.originator,
            writer,
            retry_ctx,
            |_writer| {
                responses::responses_compact(cfg, &request).map(|items| common::StreamState {
                    text: "Conversation compacted.".to_owned(),
                    compacted_input_items: items,
                    ..common::StreamState::new()
                })
            },
        ),
        _ => Err(common::LlmError::HttpStatus(
            0,
            "provider does not support remote compaction".to_owned(),
        )),
    };
    match result {
        Ok(state) => finish_stream(
            session_prompt_id,
            &prompt.originator,
            &backend_descriptor,
            state,
            None,
            writer,
        )?,
        Err(error) => finish_error(
            session_prompt_id,
            &prompt.originator,
            &backend_descriptor,
            error,
            None,
            writer,
        )?,
    }
    Ok(())
}

/// Subtract `before` from `after` (saturating, clamped to `u32`) so
/// the wire payload stays tight. The pool counters are monotonic so
/// the saturating-sub fence is purely defensive against a counter
/// reset (which shouldn't happen — the pool lives for the agent
/// process lifetime). u32 fits any realistic per-turn count.
fn compute_ws_pool_delta(
    before: responses::pool::WsPoolStats,
    after: responses::pool::WsPoolStats,
) -> tau_proto::WsPoolDelta {
    let sub = |a: u64, b: u64| u32::try_from(a.saturating_sub(b)).unwrap_or(u32::MAX);
    tau_proto::WsPoolDelta {
        upgrades: sub(after.upgrades, before.upgrades),
        silent_reconnects: sub(after.silent_reconnects, before.silent_reconnects),
        chain_strips_on_fresh: sub(after.chain_strips_on_fresh, before.chain_strips_on_fresh),
    }
}

fn finish_stream<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    backend: &tau_proto::AgentBackend,
    state: common::StreamState,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let text_empty = state.text.is_empty();
    let text_content = state.text.clone();
    let input_tokens = state.input_tokens;
    let cached_tokens = state.cached_tokens;
    let output_tokens = state.output_tokens;
    tracing::debug!(
        target: LOG_TARGET,
        session_prompt_id,
        input_tokens,
        cached_tokens,
        output_tokens,
        "agent response token usage"
    );
    let thinking = state.thinking.clone();
    let response_id = state.response_id.clone();
    let phase = state.phase;
    let mut state = state;
    let reasoning_items = std::mem::take(&mut state.reasoning_items);
    let compacted_input_items = std::mem::take(&mut state.compacted_input_items);
    let tool_calls = state.into_tool_calls();
    let thinking_empty = thinking.as_ref().is_none_or(|thinking| thinking.is_empty());
    let text = if text_empty {
        if !tool_calls.is_empty() || !thinking_empty {
            None
        } else {
            Some("(agent returned an empty response)".to_owned())
        }
    } else {
        Some(text_content)
    };
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
        AgentResponseFinished {
            session_prompt_id: session_prompt_id.into(),
            text,
            tool_calls,
            input_tokens,
            cached_tokens,
            output_tokens,
            thinking,
            // Built by the harness on the way out — see
            // handle_agent_response_finished. Agents have no view of
            // the qualified provider/model id or the running session
            // totals, so leaving this None on the wire avoids a
            // half-built struct that downstream consumers might trust.
            token_usage: None,
            originator: originator.clone(),
            backend: Some(backend.clone()),
            response_id,
            phase,
            reasoning_items,
            compacted_input_items,
            ws_pool_delta,
        },
    )))?;
    writer.flush()?;
    Ok(())
}

fn finish_error<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    backend: &tau_proto::AgentBackend,
    error: common::LlmError,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
        AgentResponseFinished {
            session_prompt_id: session_prompt_id.into(),
            text: Some(format!("LLM error: {error}")),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking: None,
            token_usage: None,
            originator: originator.clone(),
            backend: Some(backend.clone()),
            response_id: None,
            phase: None,
            reasoning_items: Vec::new(),
            compacted_input_items: Vec::new(),
            ws_pool_delta,
        },
    )))?;
    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Echo agent (for tests)
// ---------------------------------------------------------------------------

/// A simple echo agent for integration tests. Echoes user text back
/// as a `echo` tool call, or returns tool results as text.
#[cfg(feature = "echo-agent")]
pub fn run_echo<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    use tau_proto::{AgentToolCall, CborValue, ContentBlock, ConversationRole};

    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent-echo".into(),
        client_kind: ClientKind::Agent,
    })))?;
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SESSION_COMPACTION_REQUESTED),
            EventSelector::Exact(EventName::SESSION_PROMPT_CREATED),
            EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
        ],
    })))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("echo agent ready".to_owned()),
    })))?;
    writer.flush()?;

    let mut next_call = 1_u64;
    let mut prompt_snapshots: HashMap<tau_proto::SessionPromptId, tau_proto::SessionPromptCreated> =
        HashMap::new();

    loop {
        let Some(frame) = reader.read_frame()? else {
            return Ok(());
        };
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::SessionCompactionRequested(request)) => {
                let spid = request.prompt.session_prompt_id.clone();
                let prompt = match materialize_prompt(&request.prompt, &prompt_snapshots) {
                    Ok(prompt) => prompt,
                    Err(error) => {
                        writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id: spid,
                                text: Some(error),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                output_tokens: None,
                                thinking: None,
                                token_usage: None,
                                originator: request.prompt.originator.clone(),
                                backend: None,
                                response_id: None,
                                phase: None,
                                reasoning_items: Vec::new(),
                                compacted_input_items: Vec::new(),
                                ws_pool_delta: None,
                            },
                        )))?;
                        writer.flush()?;
                        continue;
                    }
                };
                writer.write_frame(&Frame::Event(Event::AgentPromptSubmitted(
                    AgentPromptSubmitted {
                        session_prompt_id: spid.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;
                writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                    AgentResponseFinished {
                        session_prompt_id: spid,
                        text: Some("Conversation compacted.".to_owned()),
                        tool_calls: Vec::new(),
                        input_tokens: None,
                        cached_tokens: None,
                        output_tokens: None,
                        thinking: None,
                        token_usage: None,
                        originator: prompt.originator.clone(),
                        backend: None,
                        response_id: None,
                        phase: None,
                        reasoning_items: Vec::new(),
                        compacted_input_items: vec![
                            serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "status": "completed",
                                "content": [{
                                    "type": "output_text",
                                    "text": "Conversation compacted.",
                                    "annotations": [],
                                    "logprobs": []
                                }]
                            })
                            .to_string(),
                        ],
                        ws_pool_delta: None,
                    },
                )))?;
                writer.flush()?;
            }
            Frame::Event(Event::SessionPromptCreated(prompt)) => {
                let spid = prompt.session_prompt_id.clone();
                let prompt = match materialize_prompt(&prompt, &prompt_snapshots) {
                    Ok(prompt) => prompt,
                    Err(error) => {
                        writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id: spid,
                                text: Some(error),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                output_tokens: None,
                                thinking: None,
                                token_usage: None,
                                originator: prompt.originator.clone(),
                                backend: None,
                                response_id: None,
                                phase: None,
                                reasoning_items: Vec::new(),
                                compacted_input_items: Vec::new(),
                                ws_pool_delta: None,
                            },
                        )))?;
                        writer.flush()?;
                        continue;
                    }
                };
                prompt_snapshots.insert(spid.clone(), prompt.clone());
                writer.write_frame(&Frame::Event(Event::AgentPromptSubmitted(
                    AgentPromptSubmitted {
                        session_prompt_id: spid.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;

                // If last message is a tool result, return it as text.
                let is_tool_result = prompt.messages.last().is_some_and(|m| {
                    m.role == ConversationRole::User
                        && m.content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                });
                if is_tool_result {
                    let text = prompt
                        .messages
                        .last()
                        .and_then(|m| {
                            m.content.iter().find_map(|b| match b {
                                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                        AgentResponseFinished {
                            session_prompt_id: spid,
                            text: Some(text),
                            tool_calls: Vec::new(),
                            input_tokens: None,
                            cached_tokens: None,
                            output_tokens: None,
                            thinking: None,
                            token_usage: None,
                            originator: prompt.originator.clone(),
                            // Echo agent never calls a real LLM backend.
                            backend: None,
                            response_id: None,
                            phase: None,
                            reasoning_items: Vec::new(),
                            compacted_input_items: Vec::new(),
                            ws_pool_delta: None,
                        },
                    )))?;
                } else {
                    // Find user text and make a tool call.
                    let user_text = prompt
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == ConversationRole::User)
                        .and_then(|m| {
                            m.content.iter().find_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                        })
                        .unwrap_or_default();

                    let call_id = format!("call-{next_call}");
                    next_call += 1;

                    let tool_call = if let Some(path) = user_text.strip_prefix("read ") {
                        AgentToolCall {
                            id: call_id.into(),
                            name: "read".into(),
                            tool_type: tau_proto::ToolType::Function,
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("path".to_owned()),
                                CborValue::Text(path.trim().to_owned()),
                            )]),
                            display: None,
                        }
                    } else if let Some(cmd) = user_text.strip_prefix("shell ") {
                        AgentToolCall {
                            id: call_id.into(),
                            name: "shell".into(),
                            tool_type: tau_proto::ToolType::Function,
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("command".to_owned()),
                                CborValue::Text(cmd.trim().to_owned()),
                            )]),
                            display: None,
                        }
                    } else {
                        AgentToolCall {
                            id: call_id.into(),
                            name: "echo".into(),
                            tool_type: tau_proto::ToolType::Function,
                            arguments: CborValue::Text(user_text),
                            display: None,
                        }
                    };

                    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                        AgentResponseFinished {
                            session_prompt_id: spid,
                            text: None,
                            tool_calls: vec![tool_call],
                            input_tokens: None,
                            cached_tokens: None,
                            output_tokens: None,
                            thinking: None,
                            token_usage: None,
                            originator: prompt.originator.clone(),
                            // Echo agent never calls a real LLM backend.
                            backend: None,
                            response_id: None,
                            phase: None,
                            reasoning_items: Vec::new(),
                            compacted_input_items: Vec::new(),
                            ws_pool_delta: None,
                        },
                    )))?;
                }
                writer.flush()?;
            }
            Frame::Message(Message::Disconnect(_)) => return Ok(()),
            _ => {}
        }
        if let Some(id) = log_id {
            writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
            writer.flush()?;
        }
    }
}

#[cfg(test)]
mod tests;
