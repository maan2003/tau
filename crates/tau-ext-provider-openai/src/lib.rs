//! OpenAI provider extension.
//!
//! The extension owns model publication and Responses execution for the
//! hardcoded `chatgpt/*` provider namespace. The harness routes prompts for
//! those models directly here; this crate emits the existing provider execution
//! events while the protocol type names are still `Agent*` for compatibility
//! inside Rust.

mod common;
mod responses;

use std::collections::{HashSet, VecDeque};
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backon::BackoffBuilder;
use tau_proto::{
    Ack, AgentBackend, AgentBackendKind, AgentBackendTransport, AgentPromptSubmitted,
    AgentResponseFinished, AgentResponseUpdated, AgentStopReason, ClientKind, ContextItem, Effort,
    Event, EventName, Frame, FrameReader, FrameWriter, Message, ModelId, ModelName,
    ProviderModelInfo, ProviderModelsUpdated, ProviderName, ThinkingSummary, Verbosity,
};
use tau_provider::storage::{self, AuthStore, Credentials, ProviderKind};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "provider-openai";

const EXTENSION_NAME: &str = "tau-ext-provider-openai";
const CHATGPT_PROVIDER_NAME: &str = "chatgpt";
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CONTEXT_WINDOW: u64 = 258400;

const CHATGPT_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"];

/// Maximum number of retry attempts before giving up on a transient provider
/// error. Combined with [`llm_retry_schedule`]'s fibonacci shape (min 10s),
/// this caps total wait time at roughly 9 minutes.
const LLM_MAX_RETRIES: usize = 8;

/// Tighter cap for extension-originated turns (delegate sub-agents,
/// notifications, etc.). These are best-effort from the user's perspective, and
/// should not block the provider extension's single prompt slot for minutes.
const LLM_MAX_RETRIES_EXTENSION: usize = 2;

/// WebSocket attempts that can fail before a ChatGPT session falls back to
/// HTTP+SSE for the process lifetime.
const WS_RETRY_BUDGET_BEFORE_HTTP_FALLBACK: usize = 2;

/// Runs the extension on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// The reader is moved to a background thread so retry-backoff sleeps can wake
/// early when the harness disconnects or sends a targeted cancel.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let startup_auth = load_auth_store();
    run_inner(reader, writer, startup_auth, load_auth_store)
}

fn load_auth_store() -> AuthStore {
    match storage::load() {
        Ok(auth) => auth,
        Err(error) => {
            tracing::warn!(
                target: LOG_TARGET,
                error = %error,
                "failed to load provider auth; publishing no models"
            );
            AuthStore::default()
        }
    }
}

#[cfg(test)]
fn run_with_auth_store<R, W>(reader: R, writer: W, auth: AuthStore) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let prompt_auth = auth.clone();
    run_inner(reader, writer, auth, move || prompt_auth.clone())
}

fn run_inner<R, W, F>(
    reader: R,
    writer: W,
    startup_auth: AuthStore,
    mut load_prompt_auth: F,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
    F: FnMut() -> AuthStore,
{
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    tau_extension::Handshake::with_kind(EXTENSION_NAME, ClientKind::Provider)
        .subscribe([
            EventName::SESSION_PROMPT_PREWARM_REQUESTED,
            EventName::UI_CANCEL_PROMPT,
        ])
        .announce_event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: models_for_auth(&startup_auth),
        }))
        .ready_message("openai provider ready")
        .run(&mut writer)?;

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
    let mut ws_pool = responses::pool::WsPool::new();
    let mut ws_disabled: HashSet<String> = HashSet::new();
    let mut canceled_spids: HashSet<tau_proto::SessionPromptId> = HashSet::new();

    loop {
        let frame = match deferred.pop_front() {
            Some(frame) => frame,
            None => match frame_rx.recv() {
                Ok(frame) => frame,
                Err(_) => return Ok(()),
            },
        };
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::SessionPromptPrewarmRequested(prewarm)) => {
                let mut auth_store = load_prompt_auth();
                handle_prewarm(&prewarm, &mut auth_store, &mut ws_pool, &mut ws_disabled);
            }
            Frame::Event(Event::SessionCompactionRequested(request)) => {
                let session_prompt_id = request.prompt.session_prompt_id.clone();
                let prompt = materialize_prompt(&request.prompt);

                if canceled_spids.remove(&session_prompt_id) {
                    finish_canceled(&session_prompt_id, &prompt.originator, &mut writer)?;
                    if let Some(id) = log_id {
                        writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                        writer.flush()?;
                    }
                    continue;
                }

                trace_prompt_like("provider compaction request", &request, &session_prompt_id);
                write_prompt_submitted(&session_prompt_id, &prompt.originator, &mut writer)?;

                let mut retry_ctx = RetryContext {
                    frame_rx: &frame_rx,
                    deferred: &mut deferred,
                    canceled_spids: &mut canceled_spids,
                };
                let mut auth_store = load_prompt_auth();
                match prompt
                    .model
                    .as_ref()
                    .and_then(|model| resolve_responses_backend(model, &mut auth_store))
                {
                    Some(backend) => handle_compaction_request(
                        &session_prompt_id,
                        &backend,
                        &prompt,
                        &mut writer,
                        &mut retry_ctx,
                    )?,
                    None => finish_missing_backend(&prompt, &session_prompt_id, &mut writer)?,
                }
            }
            Frame::Event(Event::SessionPromptCreated(prompt)) => {
                let session_prompt_id = prompt.session_prompt_id.clone();
                let prompt = materialize_prompt(&prompt);

                if canceled_spids.remove(&session_prompt_id) {
                    finish_canceled(&session_prompt_id, &prompt.originator, &mut writer)?;
                    if let Some(id) = log_id {
                        writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                        writer.flush()?;
                    }
                    continue;
                }

                trace_prompt_like("provider prompt", &prompt, &session_prompt_id);
                write_prompt_submitted(&session_prompt_id, &prompt.originator, &mut writer)?;

                let mut retry_ctx = RetryContext {
                    frame_rx: &frame_rx,
                    deferred: &mut deferred,
                    canceled_spids: &mut canceled_spids,
                };
                let mut auth_store = load_prompt_auth();
                match prompt
                    .model
                    .as_ref()
                    .and_then(|model| resolve_responses_backend(model, &mut auth_store))
                {
                    Some(backend) => handle_prompt(
                        &session_prompt_id,
                        &backend,
                        &prompt,
                        &mut writer,
                        &mut retry_ctx,
                        &mut ws_pool,
                        &mut ws_disabled,
                    )?,
                    None => finish_missing_backend(&prompt, &session_prompt_id, &mut writer)?,
                }
            }
            Frame::Event(Event::UiCancelPrompt(cancel)) => {
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

fn materialize_prompt(prompt: &tau_proto::SessionPromptCreated) -> tau_proto::SessionPromptCreated {
    let mut materialized = prompt.clone();
    materialized.tools_ref = None;
    materialized
}

fn trace_prompt_like<T: serde::Serialize>(label: &str, value: &T, session_prompt_id: &str) {
    if !tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
        return;
    }
    match serde_json::to_string_pretty(value) {
        Ok(json) => tracing::trace!(
            target: LOG_TARGET,
            session_prompt_id,
            "{label}:\n{json}"
        ),
        Err(error) => tracing::trace!(
            target: LOG_TARGET,
            session_prompt_id,
            "{label} (failed to serialize for log: {error})"
        ),
    }
}

fn write_prompt_submitted<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Event(Event::AgentPromptSubmitted(
        AgentPromptSubmitted {
            session_prompt_id: session_prompt_id.into(),
            originator: originator.clone(),
        },
    )))?;
    writer.flush()?;
    Ok(())
}

fn finish_canceled<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    tracing::info!(
        target: LOG_TARGET,
        session_prompt_id,
        "skipping provider request — already canceled by harness",
    );
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
        simple_finished(
            session_prompt_id.into(),
            originator.clone(),
            "(cancelled by harness)",
        ),
    )))?;
    writer.flush()?;
    Ok(())
}

fn finish_missing_backend<W: Write>(
    prompt: &tau_proto::SessionPromptCreated,
    session_prompt_id: &str,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let msg = match &prompt.model {
        Some(model) => format!("cannot resolve provider backend for: {model}"),
        None => "no model specified".to_owned(),
    };
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
        simple_finished(session_prompt_id.into(), prompt.originator.clone(), msg),
    )))?;
    writer.flush()?;
    Ok(())
}

fn simple_finished(
    session_prompt_id: tau_proto::SessionPromptId,
    originator: tau_proto::PromptOriginator,
    text: impl Into<String>,
) -> AgentResponseFinished {
    AgentResponseFinished {
        session_prompt_id,
        output_items: vec![common::assistant_text_item(text)],
        stop_reason: AgentStopReason::EndTurn,
        originator,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn stop_reason_from_output_items(output_items: &[ContextItem]) -> AgentStopReason {
    if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::Compaction(_)))
    {
        AgentStopReason::Compaction
    } else if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
    {
        AgentStopReason::ToolCalls
    } else {
        AgentStopReason::EndTurn
    }
}

struct RetryContext<'a> {
    frame_rx: &'a Receiver<Frame>,
    deferred: &'a mut VecDeque<Frame>,
    canceled_spids: &'a mut HashSet<tau_proto::SessionPromptId>,
}

enum SleepOutcome {
    Elapsed,
    Aborted,
}

impl RetryContext<'_> {
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
                            None => {
                                self.deferred.push_back(frame);
                                return SleepOutcome::Aborted;
                            }
                            Some(spid) if spid.as_str() == current_spid => {
                                self.deferred.push_back(frame);
                                return SleepOutcome::Aborted;
                            }
                            Some(spid) => {
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
                }
            }
        }
    }
}

fn resolve_responses_backend(
    model: &ModelId,
    auth_store: &mut AuthStore,
) -> Option<responses::ResponsesConfig> {
    if model.provider.as_str() != CHATGPT_PROVIDER_NAME {
        return None;
    }
    resolve_chatgpt_backend(model, auth_store)
}

fn resolve_chatgpt_backend(
    model: &ModelId,
    auth_store: &mut AuthStore,
) -> Option<responses::ResponsesConfig> {
    let provider = ProviderName::new(CHATGPT_PROVIDER_NAME);
    let creds = auth_store.providers.get(&provider)?.clone();
    let Credentials::Oauth {
        provider_kind,
        mut access_token,
        refresh_token,
        expires_at_ms,
        mut account_id,
    } = creds
    else {
        return None;
    };
    if provider_kind != ProviderKind::OpenaiCodex {
        return None;
    }
    if oauth_token_should_refresh(&access_token, expires_at_ms) && !refresh_token.trim().is_empty()
    {
        match tau_provider::oauth::openai_codex_refresh(&refresh_token) {
            Ok(tokens) => {
                access_token = tokens.access_token.clone();
                account_id = tokens.account_id.clone();
                let refreshed = Credentials::Oauth {
                    provider_kind: ProviderKind::OpenaiCodex,
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                    expires_at_ms: tokens.expires_at_ms,
                    account_id: tokens.account_id,
                };
                if let Err(error) = storage::save_provider(&provider, &refreshed) {
                    tracing::warn!(
                        target: LOG_TARGET,
                        provider = %provider,
                        "failed to save refreshed credentials: {error}"
                    );
                }
                auth_store.providers.insert(provider.clone(), refreshed);
            }
            Err(error) => tracing::warn!(
                target: LOG_TARGET,
                provider = %provider,
                "failed to refresh ChatGPT credentials: {error}"
            ),
        }
    }
    if access_token.trim().is_empty() {
        return None;
    }

    let model_id = model.model.as_str();
    Some(responses::ResponsesConfig {
        surface: responses::ResponsesSurface::ChatGpt,
        base_url: CHATGPT_BASE_URL.to_owned(),
        api_key: access_token,
        model_id: model_id.to_owned(),
        account_id,
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_verbosity: model_id.starts_with("gpt-5"),
        supports_phase: is_known_chatgpt_phase_capable_model_id(model_id),
        supports_encrypted_reasoning: true,
        supports_websocket: true,
        supports_compaction: true,
        supports_prompt_cache_key: true,
    })
}

fn oauth_token_should_refresh(access_token: &str, expires_at_ms: u64) -> bool {
    let now_ms = now_ms();
    if let Some(issued_at_ms) = jwt_issued_at_ms(access_token) {
        let lifetime_ms = expires_at_ms.saturating_sub(issued_at_ms);
        let refresh_at_ms = issued_at_ms.saturating_add(lifetime_ms / 2);
        if refresh_at_ms <= now_ms {
            return true;
        }
    }
    expires_at_ms <= now_ms.saturating_add(duration_millis_u64(Duration::from_secs(5 * 60)))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn jwt_issued_at_ms(jwt: &str) -> Option<u64> {
    let payload = jwt.split('.').nth(1)?;
    let payload = tau_provider::oauth::base64_url_safe_no_pad_decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims.get("iat")?.as_u64().map(|secs| secs * 1000)
}

fn is_known_chatgpt_phase_capable_model_id(model_id: &str) -> bool {
    let trimmed = model_id.trim();
    let Some(rest) = trimmed.strip_prefix("gpt-5.") else {
        return false;
    };
    let (minor, suffix) = rest.split_once('-').unwrap_or((rest, ""));
    let Ok(n) = minor.parse::<u32>() else {
        return false;
    };

    n >= 4 || (n == 3 && suffix.starts_with("codex"))
}

fn max_retries_for(originator: &tau_proto::PromptOriginator) -> usize {
    match originator {
        tau_proto::PromptOriginator::User => LLM_MAX_RETRIES,
        tau_proto::PromptOriginator::Extension { .. } => LLM_MAX_RETRIES_EXTENSION,
    }
}

fn llm_retry_schedule(max_attempts: usize) -> backon::FibonacciBackoff {
    backon::FibonacciBuilder::default()
        .with_min_delay(Duration::from_secs(10))
        .with_max_times(max_attempts)
        .with_jitter()
        .build()
}

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
            Err(error) => error,
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
            session_prompt_id,
            "provider error, retrying in {delay:?} (attempt {attempt}/{max_attempts}): {error}",
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
                session_prompt_id,
                "retry aborted by disconnect/cancel",
            );
            return Err(error);
        }
    }
}

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

struct WsRetryState {
    failures: usize,
    budget: usize,
}

struct StreamDispatchState<'a> {
    ws_pool: &'a mut responses::pool::WsPool,
    ws_disabled: &'a mut HashSet<String>,
    ws_retry: &'a mut WsRetryState,
    transport_taken: &'a mut AgentBackendTransport,
}

fn stream_with_dispatch(
    session_prompt_id: &str,
    config: &responses::ResponsesConfig,
    request: &common::PromptPayload<'_>,
    dispatch: StreamDispatchState<'_>,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<common::StreamState, common::LlmError> {
    let StreamDispatchState {
        ws_pool,
        ws_disabled,
        ws_retry,
        transport_taken,
    } = dispatch;
    let session_id = request.session_id.as_str();
    let try_ws = config.supports_websocket && !ws_disabled.contains(session_id);
    if try_ws {
        let ws_request = request_for_transport(request, AgentBackendTransport::Websocket);
        match responses::pool::run_turn_through_pool(
            ws_pool,
            config,
            session_id,
            session_prompt_id,
            &ws_request,
            on_update,
        ) {
            Ok(state) => {
                ws_retry.failures = 0;
                *transport_taken = AgentBackendTransport::Websocket;
                return Ok(state);
            }
            Err(error) if should_disable_ws_error(&error) => {
                let error = error.into_llm_error();
                tracing::warn!(
                    target: LOG_TARGET,
                    session_id,
                    "WS path failed ({error}); falling back to HTTP for this session",
                );
                ws_disabled.insert(session_id.to_owned());
            }
            Err(other) => {
                let error = other.into_llm_error();
                *transport_taken = AgentBackendTransport::Websocket;
                if error.retry_after().is_some() {
                    ws_retry.failures += 1;
                    if ws_retry.failures <= ws_retry.budget {
                        tracing::warn!(
                            target: LOG_TARGET,
                            session_id,
                            ws_retry_failures = ws_retry.failures,
                            ws_retry_budget = ws_retry.budget,
                            "WS path failed with retryable error ({error}); retrying WS before HTTP fallback",
                        );
                        return Err(error);
                    }
                    tracing::warn!(
                        target: LOG_TARGET,
                        session_id,
                        ws_retry_failures = ws_retry.failures,
                        ws_retry_budget = ws_retry.budget,
                        "WS retry budget exhausted ({error}); falling back to HTTP for this session",
                    );
                    ws_disabled.insert(session_id.to_owned());
                } else {
                    return Err(error);
                }
            }
        }
    }

    *transport_taken = AgentBackendTransport::HttpSse;
    let http_request = request_for_transport(request, AgentBackendTransport::HttpSse);
    responses::responses_stream(session_prompt_id, config, &http_request, on_update)
}

fn request_for_transport<'a>(
    request: &common::PromptPayload<'a>,
    transport: AgentBackendTransport,
) -> common::PromptPayload<'a> {
    let previous_response =
        request
            .previous_response
            .and_then(|previous_response| match previous_response.transport {
                Some(previous_transport) if previous_transport != transport => {
                    tracing::info!(
                        target: LOG_TARGET,
                        session_id = %request.session_id,
                        previous_transport = ?previous_transport,
                        current_transport = ?transport,
                        "stripping transport-incompatible previous_response_id",
                    );
                    None
                }
                _ => Some(previous_response),
            });
    common::PromptPayload {
        previous_response,
        system_prompt: request.system_prompt,
        context_items: request.context_items,
        tools: request.tools,
        params: request.params,
        tool_choice: request.tool_choice,
        originator: request.originator,
        session_id: request.session_id,
        share_user_cache_key: request.share_user_cache_key,
    }
}

fn should_disable_ws_error(error: &responses::pool::WsTurnError) -> bool {
    match error {
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
    auth_store: &mut AuthStore,
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
    let Some(config) = resolve_responses_backend(model, auth_store) else {
        tracing::debug!(
            target: LOG_TARGET,
            session_id = %prewarm.session_id,
            model = %model,
            "skipping prompt prewarm: unsupported backend",
        );
        return;
    };
    let session_id = prewarm.session_id.as_str();
    if !config.supports_websocket || ws_disabled.contains(session_id) {
        tracing::debug!(
            target: LOG_TARGET,
            session_id,
            "skipping prompt prewarm: websocket prewarm unsupported",
        );
        return;
    }

    let request = common::PromptPayload {
        system_prompt: &prewarm.system_prompt,
        context_items: &prewarm.context_items,
        tools: &prewarm.tools,
        params: prewarm.model_params,
        tool_choice: prewarm.tool_choice,
        previous_response: None,
        originator: &prewarm.originator,
        share_user_cache_key: prewarm.share_user_cache_key,
        session_id: &prewarm.session_id,
    };
    tracing::debug!(target: LOG_TARGET, session_id, "starting prompt prewarm");
    match responses::pool::run_prewarm_through_pool(ws_pool, &config, session_id, &request) {
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
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
    ws_pool: &mut responses::pool::WsPool,
    ws_disabled: &mut HashSet<String>,
) -> Result<(), Box<dyn Error>> {
    let request = common::PromptPayload {
        system_prompt: &prompt.system_prompt,
        context_items: &prompt.context_items,
        tools: &prompt.tools,
        params: prompt.model_params,
        tool_choice: prompt.tool_choice,
        previous_response: prompt.previous_response_candidate.as_ref().map(|p| {
            common::PreviousResponse {
                id: p.provider_response_id.as_str(),
                next_item_index: p.next_item_index,
                transport: Some(p.backend.transport),
            }
        }),
        originator: &prompt.originator,
        share_user_cache_key: prompt.share_user_cache_key,
        session_id: &prompt.session_id,
    };

    let originator = prompt.originator.clone();
    let mut ws_retry = WsRetryState {
        failures: 0,
        budget: max_retries_for(&originator).min(WS_RETRY_BUDGET_BEFORE_HTTP_FALLBACK),
    };
    let mut transport_taken = AgentBackendTransport::HttpSse;
    let ws_pool_before = Some(ws_pool.stats());
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
                session_prompt_id,
                config,
                &request,
                StreamDispatchState {
                    ws_pool,
                    ws_disabled,
                    ws_retry: &mut ws_retry,
                    transport_taken: &mut transport_taken,
                },
                &mut on_update,
            )
        },
    );
    let ws_pool_delta = ws_pool_before.map(|before| compute_ws_pool_delta(before, ws_pool.stats()));
    match result {
        Ok(state) => {
            let backend = backend_descriptor(config, transport_taken, state.stale_chain_fallback);
            finish_stream(
                &prompt.session_id,
                session_prompt_id,
                &prompt.originator,
                &backend,
                state,
                ws_pool_delta,
                writer,
            )?
        }
        Err(error) => {
            let backend = backend_descriptor(config, transport_taken, false);
            finish_error(
                &prompt.session_id,
                session_prompt_id,
                &prompt.originator,
                &backend,
                error,
                ws_pool_delta,
                writer,
            )?
        }
    }
    Ok(())
}

fn handle_compaction_request<W: Write>(
    session_prompt_id: &str,
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
) -> Result<(), Box<dyn Error>> {
    let request = common::PromptPayload {
        system_prompt: &prompt.system_prompt,
        context_items: &prompt.context_items,
        tools: &prompt.tools,
        params: prompt.model_params,
        tool_choice: prompt.tool_choice,
        previous_response: None,
        originator: &prompt.originator,
        share_user_cache_key: prompt.share_user_cache_key,
        session_id: &prompt.session_id,
    };
    let backend = backend_descriptor(config, AgentBackendTransport::HttpSse, false);
    let result = if config.supports_compaction {
        with_llm_retry(
            session_prompt_id,
            &prompt.originator,
            writer,
            retry_ctx,
            |_writer| {
                responses::responses_compact(config, &request).map(|items| {
                    let mut state = common::StreamState::new();
                    state.append_chat_message_delta("Conversation compacted.");
                    state.compacted_input_items = items;
                    state
                })
            },
        )
    } else {
        Err(common::LlmError::HttpStatus(
            0,
            "provider does not support remote compaction".to_owned(),
        ))
    };
    match result {
        Ok(state) => finish_stream(
            &prompt.session_id,
            session_prompt_id,
            &prompt.originator,
            &backend,
            state,
            None,
            writer,
        )?,
        Err(error) => finish_error(
            &prompt.session_id,
            session_prompt_id,
            &prompt.originator,
            &backend,
            error,
            None,
            writer,
        )?,
    }
    Ok(())
}

fn backend_descriptor(
    config: &responses::ResponsesConfig,
    transport: AgentBackendTransport,
    stale_chain_fallback: bool,
) -> AgentBackend {
    AgentBackend {
        kind: AgentBackendKind::Responses,
        base_url: config.base_url.clone(),
        transport,
        stale_chain_fallback,
    }
}

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

fn maybe_debug_write_provider_response(
    session_id: &str,
    response: &AgentResponseFinished,
    provider_terminal_event: Option<&serde_json::Value>,
) {
    let Some(backend) = response.backend.as_ref() else {
        return;
    };
    if !matches!(backend.kind, AgentBackendKind::Responses) {
        return;
    }
    let Some(dir) = responses::debug_provider_request_dir(session_id) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id,
            session_prompt_id = %response.session_prompt_id,
            "failed to create provider response debug dir: {error}",
        );
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let transport_label = match backend.transport {
        AgentBackendTransport::HttpSse => "http-sse",
        AgentBackendTransport::Websocket => "websocket",
    };
    let path = dir.join(format!(
        "{ts}-{}-{transport_label}-response.json",
        response.session_prompt_id
    ));
    let metadata = serde_json::json!({
        "session_id": session_id,
        "session_prompt_id": response.session_prompt_id,
        "transport": transport_label,
        "backend": backend,
        "provider_response_id": response.provider_response_id,
        "usage": response.usage,
        "agent_response_finished": response,
        "provider_terminal_event": provider_terminal_event,
    });
    if let Err(error) = serde_json::to_vec_pretty(&metadata)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(path, bytes))
    {
        tracing::warn!(
            target: LOG_TARGET,
            session_id,
            session_prompt_id = %response.session_prompt_id,
            "failed to write provider response debug log: {error}",
        );
    }
}

fn finish_stream<W: Write>(
    session_id: &str,
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    backend: &AgentBackend,
    mut state: common::StreamState,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let input_tokens = state.input_tokens;
    let cached_tokens = state.cached_tokens;
    let output_tokens = state.output_tokens;
    tracing::debug!(
        target: LOG_TARGET,
        session_prompt_id,
        input_tokens,
        cached_tokens,
        output_tokens,
        "provider response token usage"
    );
    let provider_terminal_event = state.provider_terminal_event.take();
    let usage = state.usage();
    let provider_response_id = state.response_id.clone();
    let mut output_items = state.into_output_items();
    if output_items.is_empty() {
        output_items.push(common::assistant_text_item(
            "(provider returned an empty response)",
        ));
    }
    let finished = AgentResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        stop_reason: stop_reason_from_output_items(&output_items),
        output_items,
        originator: originator.clone(),
        usage,
        backend: Some(backend.clone()),
        provider_response_id,
        ws_pool_delta,
    };
    maybe_debug_write_provider_response(session_id, &finished, provider_terminal_event.as_ref());
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(finished)))?;
    writer.flush()?;
    Ok(())
}

fn finish_error<W: Write>(
    session_id: &str,
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    backend: &AgentBackend,
    error: common::LlmError,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let finished = AgentResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        output_items: vec![common::assistant_text_item(format!("LLM error: {error}"))],
        stop_reason: AgentStopReason::Error,
        originator: originator.clone(),
        usage: None,
        backend: Some(backend.clone()),
        provider_response_id: None,
        ws_pool_delta,
    };
    maybe_debug_write_provider_response(session_id, &finished, None);
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(finished)))?;
    writer.flush()?;
    Ok(())
}

fn models_for_auth(auth: &AuthStore) -> Vec<ProviderModelInfo> {
    if has_chatgpt_auth(auth) {
        models_for_provider(CHATGPT_PROVIDER_NAME, CHATGPT_MODELS)
    } else {
        Vec::new()
    }
}

fn has_chatgpt_auth(auth: &AuthStore) -> bool {
    matches!(
        auth.providers.get(&ProviderName::new(CHATGPT_PROVIDER_NAME)),
        Some(Credentials::Oauth {
            provider_kind,
            access_token,
            refresh_token,
            ..
        }) if *provider_kind == ProviderKind::OpenaiCodex
            && (!access_token.trim().is_empty() || !refresh_token.trim().is_empty())
    )
}

fn models_for_provider(provider: &str, models: &[&str]) -> Vec<ProviderModelInfo> {
    models
        .iter()
        .map(|model| model_info(provider, model))
        .collect()
}

fn model_info(provider: &str, model: &str) -> ProviderModelInfo {
    ProviderModelInfo {
        id: ModelId::new(ProviderName::new(provider), ModelName::new(model)),
        display_name: None,
        context_window: CONTEXT_WINDOW,
        efforts: efforts_for_model(model),
        verbosities: verbosities_for_model(model),
        thinking_summaries: vec![
            ThinkingSummary::Off,
            ThinkingSummary::Auto,
            ThinkingSummary::Concise,
            ThinkingSummary::Detailed,
        ],
        supports_compaction: true,
    }
}

fn efforts_for_model(model: &str) -> Vec<Effort> {
    let mut efforts = vec![
        Effort::Off,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
    ];
    if supports_xhigh(model) {
        efforts.push(Effort::XHigh);
    }
    efforts
}

fn supports_xhigh(model: &str) -> bool {
    if model.contains("mini") || model.contains("nano") {
        return false;
    }
    [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gpt-5.2",
        "gpt-5.1-codex-max",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
}

fn verbosities_for_model(model: &str) -> Vec<Verbosity> {
    if model.starts_with("gpt-5") {
        vec![Verbosity::Low, Verbosity::Medium, Verbosity::High]
    } else {
        vec![Verbosity::Medium]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufReader, Cursor};

    use super::*;

    fn auth_store(entries: impl IntoIterator<Item = (&'static str, Credentials)>) -> AuthStore {
        AuthStore {
            providers: entries
                .into_iter()
                .map(|(name, credentials)| (ProviderName::new(name), credentials))
                .collect(),
        }
    }

    fn chatgpt_oauth() -> Credentials {
        Credentials::Oauth {
            provider_kind: ProviderKind::OpenaiCodex,
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at_ms: u64::MAX,
            account_id: Some("account".to_owned()),
        }
    }

    fn model_ids(models: &[ProviderModelInfo]) -> Vec<String> {
        models.iter().map(|model| model.id.to_string()).collect()
    }

    fn decode_frames(bytes: &[u8]) -> Vec<Frame> {
        let mut reader = tau_proto::FrameReader::new(BufReader::new(bytes));
        let mut frames = Vec::new();
        while let Some(frame) = reader.read_frame().expect("decode frame") {
            frames.push(frame);
        }
        frames
    }

    fn encode_frames(frames: &[Frame]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut bytes);
            for frame in frames {
                writer.write_frame(frame).expect("encode frame");
            }
            writer.flush().expect("flush frames");
        }
        bytes
    }

    fn model_id(provider: &str, model: &str) -> ModelId {
        ModelId::new(ProviderName::new(provider), ModelName::new(model))
    }

    fn prompt() -> tau_proto::SessionPromptCreated {
        tau_proto::SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            context_items: vec![ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::User,
                content: vec![tau_proto::ContentPart::Text {
                    text: "hello".to_owned(),
                }],
                phase: None,
            })],
            tools: Vec::new(),
            tools_ref: None,
            model: Some(model_id(CHATGPT_PROVIDER_NAME, "gpt-5.5")),
            model_params: Default::default(),
            tool_choice: tau_proto::ToolChoice::Auto,
            originator: tau_proto::PromptOriginator::User,
            share_user_cache_key: false,
            ctx_id: None,
            previous_response_candidate: None,
        }
    }

    #[test]
    fn no_auth_publishes_no_models() {
        // Auth presence is the first-cut enable switch: no API key or OAuth state
        // means the provider extension should still start, but advertise nothing.
        let models = models_for_auth(&AuthStore {
            providers: HashMap::new(),
        });

        assert!(models.is_empty());
    }

    #[test]
    fn chatgpt_oauth_publishes_chatgpt_models() {
        // ChatGPT/Codex is a provider namespace named `chatgpt`; there is no
        // compatibility fallback to an `openai-codex` provider name.
        let models = models_for_auth(&auth_store([(CHATGPT_PROVIDER_NAME, chatgpt_oauth())]));

        assert_eq!(
            model_ids(&models),
            vec![
                "chatgpt/gpt-5.5",
                "chatgpt/gpt-5.4",
                "chatgpt/gpt-5.4-mini",
                "chatgpt/gpt-5.3-codex"
            ]
        );
        assert!(models.iter().all(|model| model.supports_compaction));
    }

    #[test]
    fn legacy_openai_codex_name_does_not_publish_chatgpt_models() {
        // The provider rework is a hard switch. Existing `openai-codex` auth can
        // be re-added as `chatgpt`, but the new extension must not silently
        // migrate or alias it.
        let models = models_for_auth(&auth_store([("openai-codex", chatgpt_oauth())]));

        assert!(models.is_empty());
    }

    #[test]
    fn resolves_chatgpt_to_codex_responses_backend() {
        // ChatGPT is OAuth-backed and enables Codex-specific transport and replay
        // features owned by this provider slice.
        let mut auth = auth_store([(CHATGPT_PROVIDER_NAME, chatgpt_oauth())]);

        let config =
            resolve_responses_backend(&model_id(CHATGPT_PROVIDER_NAME, "gpt-5.4"), &mut auth)
                .expect("chatgpt backend");

        assert_eq!(config.surface, responses::ResponsesSurface::ChatGpt);
        assert_eq!(config.base_url, CHATGPT_BASE_URL);
        assert_eq!(config.api_key, "access");
        assert_eq!(config.account_id.as_deref(), Some("account"));
        assert!(config.supports_websocket);
        assert!(config.supports_compaction);
        assert!(config.supports_phase);
        assert!(config.supports_encrypted_reasoning);
    }

    #[test]
    fn chatgpt_phase_metadata_is_model_specific() {
        // The assistant `phase` field is only accepted by newer Codex model
        // families, so the hardcoded resolver must preserve the old whitelist.
        let mut auth = auth_store([(CHATGPT_PROVIDER_NAME, chatgpt_oauth())]);

        let old =
            resolve_responses_backend(&model_id(CHATGPT_PROVIDER_NAME, "gpt-5.2-codex"), &mut auth)
                .expect("old codex backend");
        let new =
            resolve_responses_backend(&model_id(CHATGPT_PROVIDER_NAME, "gpt-5.3-codex"), &mut auth)
                .expect("new codex backend");

        assert!(!old.supports_phase);
        assert!(new.supports_phase);
    }

    #[test]
    fn xhigh_metadata_is_model_specific() {
        // The UI cycles through the provider-published effort list, so hardcoded
        // metadata must preserve xhigh only for model families that accept it.
        let models = models_for_auth(&auth_store([(CHATGPT_PROVIDER_NAME, chatgpt_oauth())]));
        let ids_with_xhigh = models
            .iter()
            .filter(|model| model.efforts.contains(&Effort::XHigh))
            .map(|model| model.id.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            ids_with_xhigh,
            vec![
                "chatgpt/gpt-5.5",
                "chatgpt/gpt-5.4",
                "chatgpt/gpt-5.3-codex"
            ]
        );
    }

    #[test]
    fn verbosity_metadata_is_published_for_chatgpt_models() {
        // The provider snapshot is authoritative for UI cycling, so ChatGPT
        // models must publish the verbosity choices they accept.
        let models = models_for_auth(&auth_store([(CHATGPT_PROVIDER_NAME, chatgpt_oauth())]));
        let gpt = models
            .iter()
            .find(|model| model.id.to_string() == "chatgpt/gpt-5.5")
            .expect("gpt-5.5 model");

        assert_eq!(
            gpt.verbosities,
            vec![Verbosity::Low, Verbosity::Medium, Verbosity::High]
        );
    }

    #[test]
    fn run_announces_provider_models_before_ready() {
        // Provider model snapshots need to reach the harness during startup so
        // model/role UI state is available immediately after all extensions are
        // ready.
        let mut output = Vec::new();
        run_with_auth_store(
            std::io::empty(),
            &mut output,
            auth_store([(CHATGPT_PROVIDER_NAME, chatgpt_oauth())]),
        )
        .expect("run provider extension");

        let frames = decode_frames(&output);
        assert!(
            matches!(
                &frames[0],
                Frame::Message(Message::Hello(hello))
                    if hello.client_kind == ClientKind::Provider
                        && hello.client_name.as_str() == EXTENSION_NAME
            ),
            "first frame should be provider hello: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| matches!(frame, Frame::Message(Message::Subscribe(_)))),
            "provider should subscribe for prewarm/cancel events: {frames:?}"
        );
        assert!(
            frames.iter().any(|frame| matches!(
                frame,
                Frame::Event(Event::ProviderModelsUpdated(updated))
                    if model_ids(&updated.models).starts_with(&["chatgpt/gpt-5.5".to_owned()])
            )),
            "startup frames should announce provider models: {frames:?}"
        );
        assert!(
            matches!(frames.last(), Some(Frame::Message(Message::Ready(_)))),
            "last frame should be ready: {frames:?}"
        );
    }

    #[test]
    fn direct_prompt_request_with_missing_backend_is_acknowledged_and_closed() {
        // Direct provider routing must never leave the harness waiting forever,
        // even if a prompt reaches this extension without usable credentials.
        let input = encode_frames(&[
            Frame::Message(Message::LogEvent(tau_proto::LogEvent {
                id: tau_proto::LogEventId::new(7),
                recorded_at: tau_proto::UnixMicros::new(11),
                event: Box::new(Event::SessionPromptCreated(prompt())),
            })),
            Frame::Message(Message::Disconnect(tau_proto::Disconnect {
                reason: Some("done".to_owned()),
            })),
        ]);
        let mut output = Vec::new();
        run_with_auth_store(Cursor::new(input), &mut output, AuthStore::default())
            .expect("run provider extension");

        let frames = decode_frames(&output);
        let submitted = frames.iter().position(|frame| {
            matches!(
                frame,
                Frame::Event(Event::AgentPromptSubmitted(submitted))
                    if submitted.session_prompt_id.as_str() == "sp-1"
            )
        });
        let finished = frames.iter().position(|frame| {
            matches!(
                frame,
                Frame::Event(Event::AgentResponseFinished(finished))
                    if finished.session_prompt_id.as_str() == "sp-1"
                        && finished.stop_reason == AgentStopReason::EndTurn
            )
        });
        let ack = frames.iter().position(|frame| {
            matches!(
                frame,
                Frame::Message(Message::Ack(ack)) if ack.up_to.get() == 7
            )
        });

        let submitted = submitted.expect("prompt submitted event");
        let finished = finished.expect("missing-backend response finished event");
        let ack = ack.expect("ack for prompt LogEvent");
        assert!(submitted < finished, "submission should precede finish");
        assert!(finished < ack, "ack should follow prompt handling");
    }
}
