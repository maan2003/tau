//! OpenAI provider extension.
//!
//! The extension owns model publication and Responses execution for the
//! hardcoded `chatgpt/*` provider namespace. The harness routes prompts for
//! those models directly here; this crate emits the provider execution events
//! and uses provider-named protocol payload types throughout Rust.

mod common;
mod responses;

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backon::BackoffBuilder;
use dialoguer::Input;
use serde::{Deserialize, Serialize};
use tau_proto::{
    Ack, ClientKind, ContextItem, Effort, Event, EventName, Frame, FrameReader, FrameWriter,
    Message, ModelId, ModelName, ProviderBackend, ProviderBackendKind, ProviderBackendTransport,
    ProviderModelInfo, ProviderModelsUpdated, ProviderName, ProviderPromptSubmitted,
    ProviderResponseFinished, ProviderResponseUpdated, ProviderStopReason, ThinkingSummary,
    Verbosity,
};
use tau_provider::storage::AuthFile;

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "provider-openai";

const EXTENSION_NAME: &str = "tau-ext-provider-openai";
/// Auth file name for the ChatGPT/Codex provider extension.
pub const AUTH_FILE_NAME: &str = "provider-openai";
const CHATGPT_PROVIDER_NAME: &str = "chatgpt";
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CONTEXT_WINDOW: u64 = 258400;

const CHATGPT_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"];

/// OAuth credentials for the ChatGPT/Codex Responses provider.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenAiAuth {
    /// ChatGPT access token used as bearer auth for Codex Responses calls.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    /// Refresh token used to renew [`Self::access_token`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    /// Milliseconds since epoch when [`Self::access_token`] expires.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expires_at_ms: u64,
    /// OpenAI account id sent as `chatgpt-account-id`, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

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

/// Default number of provider prompts allowed to execute concurrently.
const DEFAULT_PROMPT_CONCURRENCY: usize = 4;

/// Environment override for prompt execution concurrency.
const PROMPT_CONCURRENCY_ENV: &str = "TAU_OPENAI_PROVIDER_PROMPT_CONCURRENCY";
const CANCELED_BY_HARNESS_STATUS: u16 = 499;
const CANCELED_BY_HARNESS_BODY: &str = "cancelled by harness";

/// Runs provider-specific setup commands for ChatGPT/Codex.
pub fn run_provider_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    match args.first().map(String::as_str).unwrap_or("help") {
        "login" => cmd_login()?,
        "logout" | "remove" => cmd_logout()?,
        "status" | "list" => cmd_status()?,
        "help" | "--help" | "-h" => println!("{PROVIDER_CLI_HELP}"),
        other => return Err(format!("unknown chatgpt provider subcommand: {other}").into()),
    }
    Ok(())
}

const PROVIDER_CLI_HELP: &str = "\
Usage: tau provider chatgpt <subcommand>

Subcommands:
  login        Log in / refresh ChatGPT OAuth credentials
  logout       Remove ChatGPT OAuth credentials
  status       Show ChatGPT auth status";

fn cmd_login() -> Result<(), Box<dyn Error>> {
    let auth = run_openai_codex_login()?;
    let file = AuthFile::<OpenAiAuth>::open_default(AUTH_FILE_NAME)?;
    file.save(&auth)?;
    eprintln!("\nCredentials saved to: {}", file.path().display());
    Ok(())
}

fn cmd_logout() -> Result<(), Box<dyn Error>> {
    let file = AuthFile::<OpenAiAuth>::open_default(AUTH_FILE_NAME)?;
    if file.delete()? {
        eprintln!("Removed ChatGPT credentials.");
    } else {
        eprintln!("ChatGPT credentials were not configured.");
    }
    Ok(())
}

fn cmd_status() -> Result<(), Box<dyn Error>> {
    let auth = AuthFile::<OpenAiAuth>::open_default(AUTH_FILE_NAME)?
        .load()?
        .unwrap_or_default();
    if auth.access_token.trim().is_empty() && auth.refresh_token.trim().is_empty() {
        println!("chatgpt: not configured");
    } else if now_ms() < auth.expires_at_ms {
        println!("chatgpt: logged in");
    } else {
        println!("chatgpt: expired");
    }
    Ok(())
}

fn run_openai_codex_login() -> Result<OpenAiAuth, Box<dyn Error>> {
    let (auth_url, expected_state, verifier) = tau_provider::oauth::openai_codex_auth_url();

    eprintln!("\nOpen this URL in your browser:\n");
    eprintln!("{auth_url}");
    eprintln!("\x1b]8;;{auth_url}\x1b\\Or click here.\x1b]8;;\x1b\\");
    eprintln!();
    eprintln!("After logging in, you'll be redirected to a page that won't load.");
    eprintln!("Copy the full URL from your browser's address bar and paste it here:\n");

    std::io::stdout().flush()?;
    let redirect_input: String = Input::new().with_prompt("Redirect URL").interact_text()?;

    let (code, state) = tau_provider::oauth::parse_redirect_url(&redirect_input)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    if state != expected_state {
        return Err("state mismatch — possible CSRF attack or stale URL".into());
    }

    eprintln!("Exchanging code for tokens...");
    let tokens = tau_provider::oauth::openai_codex_exchange(&code, &verifier)?;

    eprintln!("Login successful!");
    Ok(OpenAiAuth {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: tokens.expires_at_ms,
        account_id: tokens.account_id,
    })
}

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
    let startup_auth = load_auth();
    run_inner(reader, writer, startup_auth, load_auth)
}

fn load_auth() -> OpenAiAuth {
    match AuthFile::<OpenAiAuth>::open_default(AUTH_FILE_NAME).and_then(|file| file.load()) {
        Ok(Some(auth)) => auth,
        Ok(None) => OpenAiAuth::default(),
        Err(error) => {
            tracing::warn!(
                target: LOG_TARGET,
                error = %error,
                "failed to load provider auth; publishing no models"
            );
            OpenAiAuth::default()
        }
    }
}

#[cfg(test)]
fn run_with_auth<R, W>(reader: R, writer: W, auth: OpenAiAuth) -> Result<(), Box<dyn Error>>
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
    startup_auth: OpenAiAuth,
    load_prompt_auth: F,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
    F: FnMut() -> OpenAiAuth,
{
    run_inner_with_prompt_executor(
        reader,
        writer,
        startup_auth,
        load_prompt_auth,
        prompt_concurrency_limit(),
        production_prompt_executor(),
    )
}

fn run_inner_with_prompt_executor<R, W, F>(
    reader: R,
    writer: W,
    startup_auth: OpenAiAuth,
    mut load_prompt_auth: F,
    prompt_concurrency_limit: usize,
    prompt_executor: PromptExecutor,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
    F: FnMut() -> OpenAiAuth,
{
    let mut handshake_writer = FrameWriter::new(BufWriter::new(writer));

    // No past events requested: provider work starts from fresh live state.
    // Models are announced from current auth below; replaying old prompt,
    // prewarm, or cancel events would rerun or cancel completed turns.
    tau_extension::Handshake::with_kind(EXTENSION_NAME, ClientKind::Provider)
        .subscribe([
            EventName::SESSION_PROMPT_PREWARM_REQUESTED,
            EventName::UI_CANCEL_PROMPT,
        ])
        .announce_event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: models_for_auth(&startup_auth),
        }))
        .ready_message("openai provider ready")
        .run(&mut handshake_writer)?;
    let mut writer = handshake_writer.into_inner();

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

    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
    let mut deferred: VecDeque<Frame> = VecDeque::new();
    let ws_pool = Arc::new(responses::pool::SharedWsPool::new());
    let ws_disabled = Arc::new(Mutex::new(HashSet::new()));
    let cancellation = Arc::new(CancellationState::default());
    let mut prompt_queue: VecDeque<PromptJob> = VecDeque::new();
    let prompt_worker_context = PromptWorkerContext {
        worker_tx: &worker_tx,
        prompt_executor: &prompt_executor,
        cancellation: &cancellation,
        ws_pool: &ws_pool,
        ws_disabled: &ws_disabled,
    };
    let mut active_prompts = 0_usize;
    let mut ack_tracker = AckTracker::default();
    let mut input_closed = false;

    loop {
        drain_worker_messages(
            &worker_rx,
            &mut writer,
            &mut active_prompts,
            &mut ack_tracker,
        )?;
        start_queued_prompts(
            &mut prompt_queue,
            &mut active_prompts,
            prompt_concurrency_limit,
            &prompt_worker_context,
            &mut writer,
            &mut ack_tracker,
        )?;
        write_ready_acks(&mut writer, &mut ack_tracker)?;

        if input_closed && active_prompts == 0 && prompt_queue.is_empty() {
            return Ok(());
        }

        let frame = match deferred.pop_front() {
            Some(frame) => Some(frame),
            None if input_closed => None,
            None if active_prompts == 0 && prompt_queue.is_empty() => match frame_rx.recv() {
                Ok(frame) => Some(frame),
                Err(_) => {
                    input_closed = true;
                    None
                }
            },
            None => match frame_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(frame) => Some(frame),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    input_closed = true;
                    None
                }
            },
        };
        let Some(frame) = frame else {
            continue;
        };

        let (log_id, inner) = frame.peel_log();
        if let Some(id) = log_id {
            ack_tracker.register(id);
        }
        let mut complete_log_now = true;
        match inner {
            Frame::Event(Event::SessionPromptPrewarmRequested(prewarm)) => {
                let mut auth_store = load_prompt_auth();
                handle_prewarm(&prewarm, &mut auth_store, &ws_pool, &ws_disabled);
            }
            Frame::Event(Event::SessionCompactionRequested(request)) => {
                let session_prompt_id = request.prompt.session_prompt_id.clone();
                let prompt = materialize_prompt(&request.prompt);

                if cancellation.take_canceled(&session_prompt_id) {
                    let mut frame_writer = FrameWriter::new(&mut writer);
                    finish_canceled(&session_prompt_id, &prompt.originator, &mut frame_writer)?;
                    if let Some(id) = log_id {
                        ack_tracker.complete(id);
                    }
                    write_ready_acks(&mut writer, &mut ack_tracker)?;
                    continue;
                }

                trace_prompt_like("provider compaction request", &request, &session_prompt_id);
                {
                    let mut frame_writer = FrameWriter::new(&mut writer);
                    write_prompt_submitted(
                        &session_prompt_id,
                        &prompt.originator,
                        &mut frame_writer,
                    )?;
                }

                let mut retry_ctx = FrameRetryContext {
                    frame_rx: &frame_rx,
                    deferred: &mut deferred,
                    cancellation: cancellation.clone(),
                };
                let mut auth_store = load_prompt_auth();
                let mut frame_writer = FrameWriter::new(&mut writer);
                match prompt
                    .model
                    .as_ref()
                    .and_then(|model| resolve_responses_backend(model, &mut auth_store))
                {
                    Some(backend) => handle_compaction_request(
                        &session_prompt_id,
                        &backend,
                        &prompt,
                        &mut frame_writer,
                        &mut retry_ctx,
                    )?,
                    None => finish_missing_backend(&prompt, &session_prompt_id, &mut frame_writer)?,
                }
            }
            Frame::Event(Event::SessionPromptCreated(prompt)) => {
                let session_prompt_id = prompt.session_prompt_id.clone();
                let prompt = materialize_prompt(&prompt);

                if cancellation.take_canceled(&session_prompt_id) {
                    let mut frame_writer = FrameWriter::new(&mut writer);
                    finish_canceled(&session_prompt_id, &prompt.originator, &mut frame_writer)?;
                    if let Some(id) = log_id {
                        ack_tracker.complete(id);
                    }
                    write_ready_acks(&mut writer, &mut ack_tracker)?;
                    continue;
                }

                trace_prompt_like("provider prompt", &prompt, &session_prompt_id);

                let mut auth_store = load_prompt_auth();
                match prompt
                    .model
                    .as_ref()
                    .and_then(|model| resolve_responses_backend(model, &mut auth_store))
                {
                    Some(backend) => {
                        let job = PromptJob {
                            log_id,
                            session_prompt_id,
                            prompt,
                            backend,
                        };
                        if active_prompts < prompt_concurrency_limit {
                            start_prompt_job(job, &mut active_prompts, &prompt_worker_context);
                        } else {
                            prompt_queue.push_back(job);
                        }
                        complete_log_now = false;
                    }
                    None => {
                        let mut frame_writer = FrameWriter::new(&mut writer);
                        write_prompt_submitted(
                            &session_prompt_id,
                            &prompt.originator,
                            &mut frame_writer,
                        )?;
                        finish_missing_backend(&prompt, &session_prompt_id, &mut frame_writer)?;
                    }
                }
            }
            Frame::Event(Event::UiCancelPrompt(cancel)) => match cancel.session_prompt_id {
                Some(spid) => {
                    cancellation.cancel(spid.clone());
                    finish_queued_canceled(
                        &spid,
                        &mut prompt_queue,
                        &mut writer,
                        &mut ack_tracker,
                    )?;
                }
                None => cancellation.cancel_retry_sleeps(),
            },
            Frame::Message(Message::Disconnect(_)) => {
                cancellation.shutdown();
                return Ok(());
            }
            _ => {}
        }
        if complete_log_now {
            if let Some(id) = log_id {
                ack_tracker.complete(id);
            }
            write_ready_acks(&mut writer, &mut ack_tracker)?;
        }
    }
}

type PromptExecutor = Arc<dyn Fn(PromptExecution) + Send + Sync + 'static>;

struct PromptJob {
    log_id: Option<tau_proto::LogEventId>,
    session_prompt_id: tau_proto::SessionPromptId,
    prompt: tau_proto::SessionPromptCreated,
    backend: responses::ResponsesConfig,
}

struct PromptExecution {
    job: PromptJob,
    output_tx: Sender<WorkerMessage>,
    cancellation: Arc<CancellationState>,
    ws_pool: Arc<responses::pool::SharedWsPool>,
    ws_disabled: Arc<Mutex<HashSet<String>>>,
}

struct PromptWorkerContext<'a> {
    worker_tx: &'a Sender<WorkerMessage>,
    prompt_executor: &'a PromptExecutor,
    cancellation: &'a Arc<CancellationState>,
    ws_pool: &'a Arc<responses::pool::SharedWsPool>,
    ws_disabled: &'a Arc<Mutex<HashSet<String>>>,
}

impl PromptExecution {
    fn frame_writer(&self) -> FrameWriter<BufWriter<ChannelWrite>> {
        FrameWriter::new(BufWriter::new(ChannelWrite::new(self.output_tx.clone())))
    }
}

enum WorkerMessage {
    Output(Vec<u8>),
    PromptDone {
        log_id: Option<tau_proto::LogEventId>,
    },
}

struct ChannelWrite {
    tx: Sender<WorkerMessage>,
    buf: Vec<u8>,
}

impl ChannelWrite {
    fn new(tx: Sender<WorkerMessage>) -> Self {
        Self {
            tx,
            buf: Vec::new(),
        }
    }
}

impl Write for ChannelWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buf);
        self.tx
            .send(WorkerMessage::Output(bytes))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "writer closed"))
    }
}

#[derive(Default)]
struct AckTracker {
    pending: BTreeSet<u64>,
    completed: BTreeSet<u64>,
    acked_up_to: Option<u64>,
}

impl AckTracker {
    fn register(&mut self, id: tau_proto::LogEventId) {
        self.pending.insert(id.get());
    }

    fn complete(&mut self, id: tau_proto::LogEventId) {
        let raw = id.get();
        self.pending.remove(&raw);
        if self.acked_up_to.is_none_or(|acked| acked < raw) {
            self.completed.insert(raw);
        }
    }

    fn next_ack(&mut self) -> Option<tau_proto::LogEventId> {
        let limit = self.pending.first().copied();
        let raw = match limit {
            Some(first_pending) => self.completed.range(..first_pending).next_back().copied()?,
            None => self.completed.last().copied()?,
        };
        if self.acked_up_to.is_some_and(|acked| raw <= acked) {
            return None;
        }
        self.completed.retain(|completed| raw < *completed);
        self.acked_up_to = Some(raw);
        Some(tau_proto::LogEventId::new(raw))
    }
}

#[derive(Default)]
struct CancellationState {
    inner: Mutex<CancellationInner>,
    changed: Condvar,
}

#[derive(Default)]
struct CancellationInner {
    canceled_spids: HashSet<tau_proto::SessionPromptId>,
    retry_cancel_generation: u64,
    shutdown: bool,
}

impl CancellationState {
    fn cancel(&self, spid: tau_proto::SessionPromptId) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.canceled_spids.insert(spid);
            self.changed.notify_all();
        }
    }

    fn cancel_retry_sleeps(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.retry_cancel_generation = inner.retry_cancel_generation.saturating_add(1);
            self.changed.notify_all();
        }
    }

    fn shutdown(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.shutdown = true;
            self.changed.notify_all();
        }
    }

    fn take_canceled(&self, spid: &tau_proto::SessionPromptId) -> bool {
        self.inner
            .lock()
            .map(|mut inner| inner.canceled_spids.remove(spid) || inner.shutdown)
            .unwrap_or(true)
    }

    fn sleep_or_abort(&self, delay: Duration, current_spid: &str) -> SleepOutcome {
        let deadline = Instant::now() + delay;
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return SleepOutcome::Aborted,
        };
        let generation = inner.retry_cancel_generation;
        loop {
            if inner.shutdown
                || inner.retry_cancel_generation != generation
                || inner
                    .canceled_spids
                    .iter()
                    .any(|spid| spid.as_str() == current_spid)
            {
                return SleepOutcome::Aborted;
            }
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return SleepOutcome::Elapsed;
            };
            match self.changed.wait_timeout(inner, remaining) {
                Ok((guard, result)) => {
                    inner = guard;
                    if result.timed_out() {
                        return SleepOutcome::Elapsed;
                    }
                }
                Err(_) => return SleepOutcome::Aborted,
            }
        }
    }
}

fn prompt_concurrency_limit() -> usize {
    std::env::var(PROMPT_CONCURRENCY_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| 0 < value)
        .unwrap_or(DEFAULT_PROMPT_CONCURRENCY)
}

fn production_prompt_executor() -> PromptExecutor {
    Arc::new(|execution| {
        let session_prompt_id = execution.job.session_prompt_id.clone();
        let mut writer = execution.frame_writer();
        let mut retry_ctx = SharedRetryContext {
            cancellation: execution.cancellation.clone(),
        };
        let result = handle_prompt(
            &session_prompt_id,
            &execution.job.backend,
            &execution.job.prompt,
            &mut writer,
            &mut retry_ctx,
            &execution.ws_pool,
            &execution.ws_disabled,
        );
        if let Err(error) = result {
            tracing::warn!(
                target: LOG_TARGET,
                session_prompt_id = %session_prompt_id,
                "prompt worker failed to emit provider response: {error}"
            );
        }
    })
}

fn start_prompt_job(job: PromptJob, active_prompts: &mut usize, context: &PromptWorkerContext<'_>) {
    *active_prompts += 1;
    let log_id = job.log_id;
    let execution = PromptExecution {
        job,
        output_tx: context.worker_tx.clone(),
        cancellation: context.cancellation.clone(),
        ws_pool: context.ws_pool.clone(),
        ws_disabled: context.ws_disabled.clone(),
    };
    let executor = context.prompt_executor.clone();
    let done_tx = context.worker_tx.clone();
    thread::spawn(move || {
        executor(execution);
        let _ = done_tx.send(WorkerMessage::PromptDone { log_id });
    });
}

fn start_queued_prompts<W: Write>(
    prompt_queue: &mut VecDeque<PromptJob>,
    active_prompts: &mut usize,
    prompt_concurrency_limit: usize,
    context: &PromptWorkerContext<'_>,
    writer: &mut BufWriter<W>,
    ack_tracker: &mut AckTracker,
) -> Result<(), Box<dyn Error>> {
    while *active_prompts < prompt_concurrency_limit {
        let Some(job) = prompt_queue.pop_front() else {
            return Ok(());
        };
        if context.cancellation.take_canceled(&job.session_prompt_id) {
            let mut frame_writer = FrameWriter::new(&mut *writer);
            finish_canceled(
                &job.session_prompt_id,
                &job.prompt.originator,
                &mut frame_writer,
            )?;
            if let Some(id) = job.log_id {
                ack_tracker.complete(id);
            }
            continue;
        }
        start_prompt_job(job, active_prompts, context);
    }
    Ok(())
}

fn finish_queued_canceled<W: Write>(
    spid: &tau_proto::SessionPromptId,
    prompt_queue: &mut VecDeque<PromptJob>,
    writer: &mut BufWriter<W>,
    ack_tracker: &mut AckTracker,
) -> Result<(), Box<dyn Error>> {
    let Some(index) = prompt_queue
        .iter()
        .position(|job| job.session_prompt_id.as_str() == spid.as_str())
    else {
        return Ok(());
    };
    let Some(job) = prompt_queue.remove(index) else {
        return Ok(());
    };
    let mut frame_writer = FrameWriter::new(writer);
    finish_canceled(
        &job.session_prompt_id,
        &job.prompt.originator,
        &mut frame_writer,
    )?;
    if let Some(id) = job.log_id {
        ack_tracker.complete(id);
    }
    Ok(())
}

fn drain_worker_messages<W: Write>(
    worker_rx: &Receiver<WorkerMessage>,
    writer: &mut BufWriter<W>,
    active_prompts: &mut usize,
    ack_tracker: &mut AckTracker,
) -> Result<(), Box<dyn Error>> {
    loop {
        match worker_rx.try_recv() {
            Ok(WorkerMessage::Output(bytes)) => {
                writer.write_all(&bytes)?;
                writer.flush()?;
            }
            Ok(WorkerMessage::PromptDone { log_id }) => {
                *active_prompts = active_prompts.saturating_sub(1);
                if let Some(id) = log_id {
                    ack_tracker.complete(id);
                }
            }
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
    }
}

fn write_ready_acks<W: Write>(
    writer: &mut BufWriter<W>,
    ack_tracker: &mut AckTracker,
) -> Result<(), Box<dyn Error>> {
    while let Some(id) = ack_tracker.next_ack() {
        tau_proto::encode_frame(
            writer.by_ref(),
            &Frame::Message(Message::Ack(Ack { up_to: id })),
        )?;
        writer.flush()?;
    }
    Ok(())
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
    writer: &mut FrameWriter<W>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Event(Event::ProviderPromptSubmitted(
        ProviderPromptSubmitted {
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
    writer: &mut FrameWriter<W>,
) -> Result<(), Box<dyn Error>> {
    tracing::info!(
        target: LOG_TARGET,
        session_prompt_id,
        "skipping provider request — already canceled by harness",
    );
    writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(
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
    writer: &mut FrameWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let msg = match &prompt.model {
        Some(model) => format!("cannot resolve provider backend for: {model}"),
        None => "no model specified".to_owned(),
    };
    writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(
        simple_finished(session_prompt_id.into(), prompt.originator.clone(), msg),
    )))?;
    writer.flush()?;
    Ok(())
}

fn simple_finished(
    session_prompt_id: tau_proto::SessionPromptId,
    originator: tau_proto::PromptOriginator,
    text: impl Into<String>,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id,
        output_items: vec![common::assistant_text_item(text)],
        stop_reason: ProviderStopReason::EndTurn,
        originator,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn stop_reason_from_output_items(output_items: &[ContextItem]) -> ProviderStopReason {
    if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::Compaction(_)))
    {
        ProviderStopReason::Compaction
    } else if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
    {
        ProviderStopReason::ToolCalls
    } else {
        ProviderStopReason::EndTurn
    }
}

trait RetrySleeper {
    fn sleep_or_abort(&mut self, delay: Duration, current_spid: &str) -> SleepOutcome;

    fn is_aborted(&mut self, current_spid: &str) -> bool {
        matches!(
            self.sleep_or_abort(Duration::ZERO, current_spid),
            SleepOutcome::Aborted,
        )
    }
}

struct FrameRetryContext<'a> {
    frame_rx: &'a Receiver<Frame>,
    deferred: &'a mut VecDeque<Frame>,
    cancellation: Arc<CancellationState>,
}

struct SharedRetryContext {
    cancellation: Arc<CancellationState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SleepOutcome {
    Elapsed,
    Aborted,
}

impl RetrySleeper for FrameRetryContext<'_> {
    fn sleep_or_abort(&mut self, delay: Duration, current_spid: &str) -> SleepOutcome {
        let deadline = Instant::now() + delay;
        loop {
            if self
                .cancellation
                .sleep_or_abort(Duration::ZERO, current_spid)
                == SleepOutcome::Aborted
            {
                return SleepOutcome::Aborted;
            }
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
                                self.cancellation.cancel_retry_sleeps();
                                self.deferred.push_back(frame);
                                return SleepOutcome::Aborted;
                            }
                            Some(spid) if spid.as_str() == current_spid => {
                                self.cancellation.cancel(spid.clone());
                                self.deferred.push_back(frame);
                                return SleepOutcome::Aborted;
                            }
                            Some(spid) => {
                                self.cancellation.cancel(spid.clone());
                                continue;
                            }
                        }
                    }
                    let abort = matches!(&frame, Frame::Message(Message::Disconnect(_)));
                    if abort {
                        self.cancellation.shutdown();
                    }
                    self.deferred.push_back(frame);
                    if abort {
                        return SleepOutcome::Aborted;
                    }
                }
            }
        }
    }
}

impl RetrySleeper for SharedRetryContext {
    fn sleep_or_abort(&mut self, delay: Duration, current_spid: &str) -> SleepOutcome {
        // Prompt workers do not own the blocking network request, so targeted
        // cancel cannot preempt an in-flight HTTP/WS read yet. It still aborts
        // retry backoff sleeps and keeps queued prompts from starting, matching
        // the existing provider's retry-abort safety without collateral-canceling
        // unrelated prompt ids.
        self.cancellation.sleep_or_abort(delay, current_spid)
    }
}

fn resolve_responses_backend(
    model: &ModelId,
    auth_store: &mut OpenAiAuth,
) -> Option<responses::ResponsesConfig> {
    if model.provider.as_str() != CHATGPT_PROVIDER_NAME {
        return None;
    }
    resolve_chatgpt_backend(model, auth_store)
}

fn resolve_chatgpt_backend(
    model: &ModelId,
    auth_store: &mut OpenAiAuth,
) -> Option<responses::ResponsesConfig> {
    if oauth_token_should_refresh(&auth_store.access_token, auth_store.expires_at_ms)
        && !auth_store.refresh_token.trim().is_empty()
    {
        match refresh_chatgpt_credentials_locked() {
            Ok(refreshed) => {
                *auth_store = refreshed;
            }
            Err(error) => tracing::warn!(
                target: LOG_TARGET,
                "failed to refresh ChatGPT credentials: {error}"
            ),
        }
    }
    if auth_store.access_token.trim().is_empty() {
        return None;
    }

    let model_id = model.model.as_str();
    Some(responses::ResponsesConfig {
        surface: responses::ResponsesSurface::ChatGpt,
        base_url: CHATGPT_BASE_URL.to_owned(),
        api_key: auth_store.access_token.clone(),
        model_id: model_id.to_owned(),
        account_id: auth_store.account_id.clone(),
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

fn refresh_chatgpt_credentials_locked() -> std::io::Result<OpenAiAuth> {
    let auth_file = AuthFile::<OpenAiAuth>::open_default(AUTH_FILE_NAME)?;
    auth_file.with_lock(|locked| {
        let current = locked.load()?.unwrap_or_default();
        if !oauth_token_should_refresh(&current.access_token, current.expires_at_ms)
            || current.refresh_token.trim().is_empty()
        {
            return Ok(current);
        }

        let tokens = tau_provider::oauth::openai_codex_refresh(&current.refresh_token)?;
        let refreshed = OpenAiAuth {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at_ms: tokens.expires_at_ms,
            account_id: tokens.account_id,
        };
        locked.save(&refreshed)?;
        Ok(refreshed)
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

fn with_llm_retry<F, R, W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut FrameWriter<W>,
    retry_ctx: &mut R,
    mut call: F,
) -> Result<common::StreamState, common::LlmError>
where
    F: FnMut(&mut FrameWriter<W>, &mut R) -> Result<common::StreamState, common::LlmError>,
    R: RetrySleeper,
{
    let max_attempts = max_retries_for(originator);
    let mut backoff = llm_retry_schedule(max_attempts);
    let mut attempt = 0_usize;
    loop {
        let error = match call(writer, retry_ctx) {
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
    writer: &mut FrameWriter<W>,
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
    let _ = writer.write_frame(&Frame::Event(Event::ProviderResponseUpdated(
        ProviderResponseUpdated {
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

struct SharedStreamDispatchState<'a> {
    ws_pool: &'a responses::pool::SharedWsPool,
    ws_disabled: &'a Mutex<HashSet<String>>,
    ws_retry: &'a mut WsRetryState,
    transport_taken: &'a mut ProviderBackendTransport,
}

fn stream_with_shared_dispatch(
    session_prompt_id: &str,
    config: &responses::ResponsesConfig,
    request: &common::PromptPayload<'_>,
    dispatch: SharedStreamDispatchState<'_>,
    should_abort: &mut impl FnMut() -> bool,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<common::StreamState, common::LlmError> {
    let SharedStreamDispatchState {
        ws_pool,
        ws_disabled,
        ws_retry,
        transport_taken,
    } = dispatch;
    let session_id = request.session_id.as_str();
    let try_ws = config.supports_websocket
        && ws_disabled
            .lock()
            .map(|disabled| !disabled.contains(session_id))
            .unwrap_or(false);
    if try_ws {
        let ws_request = request_for_transport(request, ProviderBackendTransport::Websocket);
        match responses::pool::run_turn_through_shared_pool(
            ws_pool,
            config,
            session_prompt_id,
            &ws_request,
            should_abort,
            on_update,
        ) {
            Ok(state) => {
                ws_retry.failures = 0;
                *transport_taken = ProviderBackendTransport::Websocket;
                return Ok(state);
            }
            Err(error) if should_disable_ws_error(&error) => {
                let error = error.into_llm_error();
                tracing::warn!(
                    target: LOG_TARGET,
                    session_id,
                    "WS path failed ({error}); falling back to HTTP for this session",
                );
                if let Ok(mut disabled) = ws_disabled.lock() {
                    disabled.insert(session_id.to_owned());
                }
            }
            Err(other) => {
                let error = other.into_llm_error();
                *transport_taken = ProviderBackendTransport::Websocket;
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
                    if let Ok(mut disabled) = ws_disabled.lock() {
                        disabled.insert(session_id.to_owned());
                    }
                } else {
                    return Err(error);
                }
            }
        }
    }

    *transport_taken = ProviderBackendTransport::HttpSse;
    let http_request = request_for_transport(request, ProviderBackendTransport::HttpSse);
    responses::responses_stream(session_prompt_id, config, &http_request, on_update)
}

fn request_for_transport<'a>(
    request: &common::PromptPayload<'a>,
    transport: ProviderBackendTransport,
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
        responses::pool::WsTurnError::Canceled => false,
        responses::pool::WsTurnError::Other(error) => should_disable_ws(error),
    }
}

fn is_canceled_by_harness(error: &common::LlmError) -> bool {
    matches!(
        error,
        common::LlmError::HttpStatus(CANCELED_BY_HARNESS_STATUS, body)
            if body == CANCELED_BY_HARNESS_BODY
    )
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
    auth_store: &mut OpenAiAuth,
    ws_pool: &responses::pool::SharedWsPool,
    ws_disabled: &Mutex<HashSet<String>>,
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
    let ws_disabled_for_session = ws_disabled
        .lock()
        .map(|disabled| disabled.contains(session_id))
        .unwrap_or(true);
    if !config.supports_websocket || ws_disabled_for_session {
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
    match responses::pool::run_prewarm_through_shared_pool(ws_pool, &config, session_id, &request) {
        Ok(_) => tracing::debug!(target: LOG_TARGET, session_id, "completed prompt prewarm"),
        Err(error) if should_disable_ws(&error) => {
            tracing::debug!(
                target: LOG_TARGET,
                session_id,
                "prompt prewarm disabled WS path: {error}",
            );
            if let Ok(mut disabled) = ws_disabled.lock() {
                disabled.insert(session_id.to_owned());
            }
        }
        Err(error) => tracing::debug!(
            target: LOG_TARGET,
            session_id,
            "prompt prewarm failed: {error}",
        ),
    }
}

fn handle_prompt<R, W: Write>(
    session_prompt_id: &str,
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<W>,
    retry_ctx: &mut R,
    ws_pool: &responses::pool::SharedWsPool,
    ws_disabled: &Mutex<HashSet<String>>,
) -> Result<(), Box<dyn Error>>
where
    R: RetrySleeper,
{
    write_prompt_submitted(session_prompt_id, &prompt.originator, writer)?;
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
    let mut transport_taken = ProviderBackendTransport::HttpSse;
    let ws_pool_before = ws_pool.stats();
    let result = with_llm_retry(
        session_prompt_id,
        &originator,
        writer,
        retry_ctx,
        |writer, retry_ctx| {
            let mut on_update = |text_so_far: &str, thinking_so_far: Option<&str>| {
                let _ = writer.write_frame(&Frame::Event(Event::ProviderResponseUpdated(
                    ProviderResponseUpdated {
                        session_prompt_id: session_prompt_id.into(),
                        text: text_so_far.to_owned(),
                        thinking: thinking_so_far.map(str::to_owned),
                        originator: originator.clone(),
                    },
                )));
                let _ = writer.flush();
            };
            stream_with_shared_dispatch(
                session_prompt_id,
                config,
                &request,
                SharedStreamDispatchState {
                    ws_pool,
                    ws_disabled,
                    ws_retry: &mut ws_retry,
                    transport_taken: &mut transport_taken,
                },
                &mut || retry_ctx.is_aborted(session_prompt_id),
                &mut on_update,
            )
        },
    );
    let ws_pool_delta = ws_pool_before.and_then(|before| {
        ws_pool
            .stats()
            .map(|after| compute_ws_pool_delta(before, after))
    });
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
        Err(error) if is_canceled_by_harness(&error) => {
            finish_canceled(session_prompt_id, &prompt.originator, writer)?
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

fn handle_compaction_request<R, W: Write>(
    session_prompt_id: &str,
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<W>,
    retry_ctx: &mut R,
) -> Result<(), Box<dyn Error>>
where
    R: RetrySleeper,
{
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
    let backend = backend_descriptor(config, ProviderBackendTransport::HttpSse, false);
    let result = if config.supports_compaction {
        with_llm_retry(
            session_prompt_id,
            &prompt.originator,
            writer,
            retry_ctx,
            |_writer, _retry_ctx| {
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
    transport: ProviderBackendTransport,
    stale_chain_fallback: bool,
) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::Responses,
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
    response: &ProviderResponseFinished,
    provider_terminal_event: Option<&serde_json::Value>,
) {
    let Some(backend) = response.backend.as_ref() else {
        return;
    };
    if !matches!(backend.kind, ProviderBackendKind::Responses) {
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
        ProviderBackendTransport::HttpSse => "http-sse",
        ProviderBackendTransport::Websocket => "websocket",
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
        "provider_response_finished": response,
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
    backend: &ProviderBackend,
    mut state: common::StreamState,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    writer: &mut FrameWriter<W>,
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
    let finished = ProviderResponseFinished {
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
    writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(finished)))?;
    writer.flush()?;
    Ok(())
}

fn finish_error<W: Write>(
    session_id: &str,
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    backend: &ProviderBackend,
    error: common::LlmError,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    writer: &mut FrameWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let finished = ProviderResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        output_items: vec![common::assistant_text_item(format!("LLM error: {error}"))],
        stop_reason: ProviderStopReason::Error,
        originator: originator.clone(),
        usage: None,
        backend: Some(backend.clone()),
        provider_response_id: None,
        ws_pool_delta,
    };
    maybe_debug_write_provider_response(session_id, &finished, None);
    writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(finished)))?;
    writer.flush()?;
    Ok(())
}

fn models_for_auth(auth: &OpenAiAuth) -> Vec<ProviderModelInfo> {
    if has_chatgpt_auth(auth) {
        models_for_provider(CHATGPT_PROVIDER_NAME, CHATGPT_MODELS)
    } else {
        Vec::new()
    }
}

fn has_chatgpt_auth(auth: &OpenAiAuth) -> bool {
    !auth.access_token.trim().is_empty() || !auth.refresh_token.trim().is_empty()
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
        default_affinity: default_affinity_for_model(model),
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

fn default_affinity_for_model(model: &str) -> i32 {
    match model {
        "gpt-5.5" => 400,
        "gpt-5.4" => 300,
        "gpt-5.3-codex" => 200,
        "gpt-5.4-mini" => 100,
        _ => 0,
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
    use std::io::{BufReader, Cursor};
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    use super::*;

    fn chatgpt_auth() -> OpenAiAuth {
        OpenAiAuth {
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
        let models = models_for_auth(&OpenAiAuth::default());

        assert!(models.is_empty());
    }

    #[test]
    fn chatgpt_oauth_publishes_chatgpt_models() {
        // ChatGPT/Codex is a provider namespace named `chatgpt`; there is no
        // compatibility fallback to an `openai-codex` provider name.
        let models = models_for_auth(&chatgpt_auth());

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
    fn resolves_chatgpt_to_codex_responses_backend() {
        // ChatGPT is OAuth-backed and enables Codex-specific transport and replay
        // features owned by this provider slice.
        let mut auth = chatgpt_auth();

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
        let mut auth = chatgpt_auth();

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
        let models = models_for_auth(&chatgpt_auth());
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
        let models = models_for_auth(&chatgpt_auth());
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
    fn ack_tracker_waits_for_contiguous_completed_log_events() {
        // Parallel prompt workers can finish out of order, but `Ack { up_to }`
        // is cumulative. Do not ack a later prompt until earlier received log
        // events have completed, or a crash could lose accepted work.
        let mut tracker = AckTracker::default();
        tracker.register(tau_proto::LogEventId::new(7));
        tracker.register(tau_proto::LogEventId::new(8));

        tracker.complete(tau_proto::LogEventId::new(8));
        assert_eq!(tracker.next_ack(), None);

        tracker.complete(tau_proto::LogEventId::new(7));
        assert_eq!(tracker.next_ack(), Some(tau_proto::LogEventId::new(8)));
        assert_eq!(tracker.next_ack(), None);
    }

    #[test]
    fn prompt_workers_start_concurrently() {
        // Regression coverage for backend-agent parallelism: two accepted
        // provider prompts must both enter worker execution before the first
        // one finishes. A serial dispatcher would time out the first worker's
        // wait and never observe two active starts at once.
        let mut first = prompt();
        first.session_prompt_id = "sp-par-1".into();
        let mut second = prompt();
        second.session_prompt_id = "sp-par-2".into();
        let input = encode_frames(&[
            Frame::Message(Message::LogEvent(tau_proto::LogEvent {
                id: tau_proto::LogEventId::new(7),
                recorded_at: tau_proto::UnixMicros::new(11),
                event: Box::new(Event::SessionPromptCreated(first)),
            })),
            Frame::Message(Message::LogEvent(tau_proto::LogEvent {
                id: tau_proto::LogEventId::new(8),
                recorded_at: tau_proto::UnixMicros::new(12),
                event: Box::new(Event::SessionPromptCreated(second)),
            })),
        ]);
        let started = std::sync::Arc::new((Mutex::new((0_usize, 0_usize)), Condvar::new()));
        let executor_started = started.clone();
        let executor: PromptExecutor = std::sync::Arc::new(move |execution| {
            let session_prompt_id = execution.job.session_prompt_id.clone();
            let originator = execution.job.prompt.originator.clone();
            let (lock, cv) = &*executor_started;
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut guard = lock.lock().expect("started lock");
            guard.0 += 1;
            guard.1 = guard.1.max(guard.0);
            cv.notify_all();
            while guard.0 < 2 {
                let now = Instant::now();
                let Some(remaining) = deadline.checked_duration_since(now) else {
                    break;
                };
                let (next, wait) = cv.wait_timeout(guard, remaining).expect("wait for peer");
                guard = next;
                if wait.timed_out() {
                    break;
                }
            }
            drop(guard);

            let mut writer = execution.frame_writer();
            write_prompt_submitted(&session_prompt_id, &originator, &mut writer)
                .expect("submitted");
            writer
                .write_frame(&Frame::Event(Event::ProviderResponseFinished(
                    simple_finished(session_prompt_id.clone(), originator, "done"),
                )))
                .expect("finished");
            writer.flush().expect("flush fake response");

            let mut guard = lock.lock().expect("started lock");
            guard.0 -= 1;
            cv.notify_all();
        });

        let auth = chatgpt_auth();
        let prompt_auth = auth.clone();
        let mut output = Vec::new();
        run_inner_with_prompt_executor(
            Cursor::new(input),
            &mut output,
            auth,
            move || prompt_auth.clone(),
            2,
            executor,
        )
        .expect("run provider extension");

        let max_started = started.0.lock().expect("started lock").1;
        assert_eq!(max_started, 2, "both prompt workers should overlap");
        let frames = decode_frames(&output);
        let finished_count = frames
            .iter()
            .filter(|frame| matches!(frame, Frame::Event(Event::ProviderResponseFinished(_))))
            .count();
        assert_eq!(finished_count, 2);
        assert!(frames.iter().any(|frame| {
            matches!(frame, Frame::Message(Message::Ack(ack)) if ack.up_to.get() == 8)
        }));
    }

    #[test]
    fn run_announces_provider_models_before_ready() {
        // Provider model snapshots need to reach the harness during startup so
        // model/role UI state is available immediately after all extensions are
        // ready.
        let mut output = Vec::new();
        run_with_auth(std::io::empty(), &mut output, chatgpt_auth())
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
        run_with_auth(Cursor::new(input), &mut output, OpenAiAuth::default())
            .expect("run provider extension");

        let frames = decode_frames(&output);
        let submitted = frames.iter().position(|frame| {
            matches!(
                frame,
                Frame::Event(Event::ProviderPromptSubmitted(submitted))
                    if submitted.session_prompt_id.as_str() == "sp-1"
            )
        });
        let finished = frames.iter().position(|frame| {
            matches!(
                frame,
                Frame::Event(Event::ProviderResponseFinished(finished))
                    if finished.session_prompt_id.as_str() == "sp-1"
                        && finished.stop_reason == ProviderStopReason::EndTurn
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
