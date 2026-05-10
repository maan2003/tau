//! Bridge agent prompt-start / response-finish events into iTerm2-style
//! OSC 1337 `SetUserVar` notifications, mirroring the dpc-personal
//! `notification-sounds.ts` and `user-text-notification.sh` Pi
//! extensions.
//!
//! Events emitted (all via `Osc1337SetUserVar`):
//! - `ui.prompt_submitted` → `user-notification = protoss-probe-ack`
//! - final `agent.response_finished` (only when `tool_calls` is empty) →
//!   `user-notification = protoss-upgrade-complete`
//! - After `idle_seconds` (default 60) of inactivity following a final response
//!   → an `ExtAgentQuery` side-prompt to the agent asking for a one-sentence
//!   summary; when the matching `ExtAgentQueryResult` arrives (or a 10s
//!   fallback timer expires) → `user-text-notification = {"urgency": "normal",
//!   "title": "Agent idle: <host>:<cwd>", "body": "<summary or fallback>",
//!   "app_name": "tau"}`. `app_name` follows the schema
//!   `user-text-notification.sh` emits so downstream consumers can use it as
//!   the desktop notification's source-app indicator instead of us baking it
//!   into the title. The idle timer resets on every user-originated
//!   `ui.prompt_submitted` / `agent.prompt_submitted`. Tunable via the
//!   extension's `config.idle_seconds` field in `harness.json5`.
//!
//! The downstream tooling (typically a terminal multiplexer status
//! line or a `user-notification.sh` consumer wired to a sound file)
//! is what actually plays the sounds / pops the desktop notification;
//! this extension just publishes the user-var change so a UI further
//! up the stack can forward it to the terminal.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tau_proto::{
    ClientKind, ConfigError, Event, EventSelector, ExtAgentQuery, Frame, FrameReader, FrameWriter,
    Hello, Message, Osc1337SetUserVar, PROTOCOL_VERSION, Ready, Subscribe,
};

/// `tracing` target for events emitted from this extension. Matches
/// the convention described in [`tau_extension`]: a short identifier
/// the user can name in `TAU_LOG=std-notifications=trace`.
pub const LOG_TARGET: &str = "std-notifications";

/// User-var name for sound notifications (matches `user-notification.sh`).
pub const SOUND_VAR_NAME: &str = "user-notification";

/// User-var name for text/desktop notifications (matches
/// `user-text-notification.sh`).
pub const TEXT_VAR_NAME: &str = "user-text-notification";

/// `app_name` field on the text-notification payload. Mirrors what
/// `user-text-notification.sh`'s `NOTIFY_APP_NAME` env var sets, so
/// downstream consumers can route or filter notifications by source
/// app and surface the source in the desktop notification UI without
/// us having to re-state it inside the title text.
pub const NOTIFY_APP_NAME: &str = "tau";

/// Sound key emitted when the user submits a prompt.
pub const VALUE_AGENT_START: &str = "protoss-probe-ack";

/// Sound key emitted at the end of an agent turn.
pub const VALUE_AGENT_END: &str = "protoss-upgrade-complete";

/// Default idle window before the extension nudges the user via a
/// text notification, in seconds. Override via the `idle_seconds`
/// field of the extension's `config` block in `harness.json5`.
pub const DEFAULT_IDLE_SECONDS: u64 = 60;

/// How long to wait for the agent to summarize the conversation
/// before falling back to the static idle text. Once the idle window
/// has elapsed we want to actually notify the user soon, even if the
/// agent is wedged or the model is unreachable.
pub const SUMMARY_TIMEOUT_SECONDS: u64 = 10;

/// Instruction sent to the agent as a side prompt when the idle
/// timer fires. Mirrors the prompt Pi's `idle-notification.ts` uses,
/// adapted for our harness-mediated query path.
const SUMMARY_INSTRUCTION: &str = "Summarize in one short sentence: what \
is the last thing you did or what do you need from the user now? Keep it \
under 200 characters. Output only the summary, nothing else.";

/// Static fallback body used when the summary request errors out,
/// returns empty text, or doesn't arrive within
/// [`SUMMARY_TIMEOUT_SECONDS`]. Matches Pi's
/// `idle-notification.ts` so downstream `user-text-notification.sh`
/// consumers see the same wording across the two implementations.
const FALLBACK_BODY: &str = "Waiting for user input";

/// Returns the system hostname via `gethostname(2)`. Falls back to
/// `"host"` if the syscall fails or the bytes aren't UTF-8.
fn hostname() -> String {
    // Safety: `gethostname` writes at most `buf.len()` bytes into
    // `buf` and POSIX guarantees NUL termination on success when
    // the result fits.
    let mut buf = [0_u8; 256];
    #[allow(unsafe_code)]
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return "host".to_owned();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..len])
        .ok()
        .map(str::to_owned)
        .unwrap_or_else(|| "host".to_owned())
}

/// Build the notification title: `Agent idle: <host>:<basename(cwd)>`.
/// Mirrors Pi's `idle-notification.ts` so the wording matches across
/// both implementations of the extension.
fn build_title() -> String {
    let host = hostname();
    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd_short = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
    let cwd_short = if cwd_short.is_empty() {
        cwd.to_string_lossy().into_owned()
    } else {
        cwd_short
    };
    format!("Agent idle: {host}:{cwd_short}")
}

/// Phase of the idle-watch state machine. `WaitingIdle` is the base
/// "agent finished, count down to nudge" state. When it elapses we
/// send a side-query to the agent for a one-sentence summary and
/// transition to `WaitingSummary`; whichever of (result, timeout)
/// arrives first decides what body the user sees.
enum IdleState {
    WaitingIdle { deadline: Instant },
    WaitingSummary { query_id: String, deadline: Instant },
}

impl IdleState {
    fn deadline(&self) -> Instant {
        match self {
            Self::WaitingIdle { deadline } | Self::WaitingSummary { deadline, .. } => *deadline,
        }
    }
}

/// User-supplied configuration for this extension. See the crate's
/// `README.md` for the full schema and worked examples.
#[derive(serde::Deserialize, Debug, Default, Clone)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Idle window, in seconds, before the extension nudges the
    /// user. `None` keeps the [`DEFAULT_IDLE_SECONDS`] default. The
    /// wire format is integer seconds; for sub-second test windows
    /// the test entry points take a `Duration` directly.
    idle_seconds: Option<u64>,
    /// Optional argv to invoke whenever the extension would normally
    /// emit the OSC text notification (idle-summary or fallback). The
    /// command runs *in addition to* the OSC, never instead of it,
    /// so existing terminal-side consumers keep working.
    ///
    /// Calling convention mirrors `user-text-notification.sh`:
    /// - `argv[0]` is the program; the title is appended as the next argument
    ///   (`argv[1]` if no extra args, otherwise the first trailing arg).
    /// - The body is piped to the command's stdin.
    /// - `NOTIFY_URGENCY=normal` and `NOTIFY_APP_NAME=tau` are set in the
    ///   child's environment.
    ///
    /// Spawned detached: we don't wait for it, and stdout/stderr are
    /// silently discarded. A failing command logs at `warn` and is
    /// otherwise ignored.
    idle_command: Option<Vec<String>>,
}

impl ExtConfig {
    fn idle_duration(&self) -> Duration {
        Duration::from_secs(self.idle_seconds.unwrap_or(DEFAULT_IDLE_SECONDS))
    }
}

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging();
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    run_with_idle(reader, writer, Duration::from_secs(DEFAULT_IDLE_SECONDS))
}

/// Inbound message on the main thread's channel: either a decoded
/// frame from the reader thread, or a terminal condition that ends
/// the loop.
enum InMsg {
    Frame(Box<Frame>),
    EndOfStream,
}

/// Test-friendly entry point. Lets unit tests drop the idle window
/// to a few hundred milliseconds so the timeout path is observable
/// without slowing the suite. Uses [`SUMMARY_TIMEOUT_SECONDS`] for
/// the summary fallback timer; tests that exercise the fallback path
/// directly should call [`run_with_idle_and_summary_timeout`] with a
/// shorter summary timeout instead.
pub fn run_with_idle<R, W>(
    reader: R,
    writer: W,
    idle_duration: Duration,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    run_with_idle_and_summary_timeout(
        reader,
        writer,
        idle_duration,
        Duration::from_secs(SUMMARY_TIMEOUT_SECONDS),
    )
}

/// Test-friendly entry point with an overridable summary fallback
/// timeout. Useful for exercising the wedged-agent path without
/// blocking the test suite for [`SUMMARY_TIMEOUT_SECONDS`] seconds.
pub fn run_with_idle_and_summary_timeout<R, W>(
    reader: R,
    writer: W,
    mut idle_duration: Duration,
    summary_timeout: Duration,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut writer = FrameWriter::new(BufWriter::new(writer));
    // Live config — only `idle_command` actually rides on it past
    // the parse: `idle_seconds` is reflected into the local
    // `idle_duration` Duration so we keep sub-second test precision
    // (the wire schema is integer seconds, which is fine for users
    // but rounds out millisecond test windows). `Configure`
    // from the harness overwrites both on receipt.
    let mut config = ExtConfig::default();

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-std-notifications".into(),
        client_kind: ClientKind::Tool,
    })))?;
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
            EventSelector::Exact(tau_proto::EventName::AGENT_RESPONSE_FINISHED),
            EventSelector::Exact(tau_proto::EventName::UI_PROMPT_SUBMITTED),
            // Trailing-edge debounced typing pings from the UI:
            // bumps the idle deadline so the desktop notification
            // doesn't fire while the user is mid-sentence.
            EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
            // Side-query results come back point-to-point from the
            // harness, but we subscribe defensively so the broadcast
            // form (if it ever appears) also reaches us.
            EventSelector::Exact(tau_proto::EventName::EXTENSION_AGENT_QUERY_RESULT),
        ],
    })))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("std-notifications ready".to_owned()),
    })))?;
    writer.flush()?;

    // Spawn a reader thread so the main loop can wait on either an
    // incoming frame or an idle deadline via `recv_timeout`. The
    // reader exits naturally when stdin closes, then the channel
    // disconnects and the main loop sees EndOfStream.
    let (tx, rx) = mpsc::channel::<InMsg>();
    let _reader_handle = thread::spawn(move || {
        let mut reader = FrameReader::new(BufReader::new(reader));
        loop {
            match reader.read_frame() {
                Ok(Some(frame)) => {
                    if tx.send(InMsg::Frame(Box::new(frame))).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(InMsg::EndOfStream);
                    break;
                }
                Err(_) => {
                    // Treat decode errors as end-of-stream. The
                    // socket layer above will surface the failure
                    // through its own channels.
                    let _ = tx.send(InMsg::EndOfStream);
                    break;
                }
            }
        }
    });

    let mut idle: Option<IdleState> = None;
    let mut input_closed = false;
    let mut waiting_for_final_response = false;
    let mut next_query_id: u64 = 0;
    loop {
        let recv_result = match (idle.as_ref().map(IdleState::deadline), input_closed) {
            (Some(deadline), false) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                rx.recv_timeout(wait)
            }
            (None, false) => match rx.recv() {
                Ok(msg) => Ok(msg),
                Err(_) => Err(mpsc::RecvTimeoutError::Disconnected),
            },
            // Input closed but a notification is still pending: the
            // output side (the UI / terminal) is independent, so
            // honor the deadline instead of dropping the warning.
            // `recv_timeout` on a disconnected channel returns
            // immediately, so explicitly sleep instead.
            (Some(deadline), true) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    thread::sleep(wait);
                }
                Err(mpsc::RecvTimeoutError::Timeout)
            }
            (None, true) => break,
        };

        match recv_result {
            Ok(InMsg::Frame(frame)) => {
                let (_, inner) = frame.peel_log();
                // Handle messages first.
                match inner {
                    Frame::Message(Message::Configure(msg)) => {
                        match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                            Ok(cfg) => {
                                idle_duration = cfg.idle_duration();
                                tracing::info!(
                                    target: LOG_TARGET,
                                    idle_seconds = idle_duration.as_secs(),
                                    has_idle_command = cfg.idle_command.is_some(),
                                    "applied config",
                                );
                                config = cfg;
                            }
                            Err(message) => {
                                tracing::warn!(
                                    target: LOG_TARGET,
                                    error = %message,
                                    "rejecting config",
                                );
                                writer.write_frame(&Frame::Message(Message::ConfigError(
                                    ConfigError {
                                        message: message.clone(),
                                    },
                                )))?;
                                writer.flush()?;
                            }
                        }
                        continue;
                    }
                    Frame::Message(Message::Disconnect(_)) => {
                        tracing::info!(target: LOG_TARGET, "disconnect received, exiting");
                        break;
                    }
                    Frame::Message(_) => continue,
                    Frame::Event(_) => {}
                }
                let Frame::Event(inner) = inner else {
                    continue;
                };
                tracing::trace!(target: LOG_TARGET, name = %inner.name(), "event received");
                // Sub-agent (`PromptOriginator::Extension`) events
                // share the bus with the user's interactive turn, but
                // notifications must only react to the *main* agent.
                // Reacting to a side conversation's prompt/response
                // would clear the user's idle deadline (or fire the
                // end-of-turn chime) on activity that's invisible to
                // the user. Filter once, at the top, so new event
                // variants can't accidentally leak sub-agent activity
                // through a per-branch oversight.
                if is_sub_agent_event(&inner) {
                    tracing::trace!(
                        target: LOG_TARGET,
                        name = %inner.name(),
                        "skipping sub-agent event",
                    );
                    continue;
                }
                match inner {
                    Event::AgentPromptSubmitted(_submitted) => {
                        idle = None;
                    }
                    Event::UiPromptSubmitted(_prompt) => {
                        idle = None;
                        if !waiting_for_final_response {
                            writer.write_frame(&Frame::Event(sound_event(VALUE_AGENT_START)))?;
                            writer.flush()?;
                            waiting_for_final_response = true;
                        }
                    }
                    Event::UiPromptDraft(_) => {
                        // The user is mid-typing — push the idle
                        // deadline back so the desktop notification
                        // doesn't fire while they're composing. Only
                        // applies in `WaitingIdle`; if we've already
                        // dispatched a side-query summarization
                        // (`WaitingSummary`), let it complete
                        // normally so the side conversation isn't
                        // billed for nothing. TODO: when prompt
                        // cancellation lands, cancel the in-flight
                        // side query here too.
                        if let Some(IdleState::WaitingIdle { deadline }) = idle.as_mut() {
                            *deadline = Instant::now() + idle_duration;
                            tracing::trace!(
                                target: LOG_TARGET,
                                "extended idle deadline on prompt draft",
                            );
                        }
                    }
                    Event::AgentResponseFinished(finished) => {
                        // The agent emits one `AgentResponseFinished`
                        // per LLM call. When `tool_calls` is non-empty,
                        // the harness will run the tools and feed the
                        // results back as a new prompt — the *turn*
                        // isn't actually done yet. Only fire the
                        // end-of-turn sound + idle timer when the
                        // agent returned a final answer with no
                        // pending tool work. (Sub-agent finishes are
                        // already filtered out at the top of the
                        // dispatch loop.)
                        if !finished.tool_calls.is_empty() {
                            tracing::trace!(
                                target: LOG_TARGET,
                                tool_calls = finished.tool_calls.len(),
                                "skipping mid-turn AgentResponseFinished",
                            );
                            continue;
                        }
                        writer.write_frame(&Frame::Event(sound_event(VALUE_AGENT_END)))?;
                        writer.flush()?;
                        waiting_for_final_response = false;
                        idle = Some(IdleState::WaitingIdle {
                            deadline: Instant::now() + idle_duration,
                        });
                        tracing::debug!(
                            target: LOG_TARGET,
                            seconds = idle_duration.as_secs(),
                            "idle deadline armed",
                        );
                    }
                    Event::ExtAgentQueryResult(result) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            query_id = %result.query_id,
                            text_len = result.text.len(),
                            error = ?result.error,
                            idle_state = match &idle {
                                None => "none",
                                Some(IdleState::WaitingIdle { .. }) => "waiting_idle",
                                Some(IdleState::WaitingSummary { .. }) => "waiting_summary",
                            },
                            "received ExtAgentQueryResult",
                        );
                        // Match against the in-flight query id; ignore
                        // stragglers from cancelled / superseded
                        // requests.
                        if let Some(IdleState::WaitingSummary { query_id, .. }) = idle.as_ref()
                            && result.query_id == *query_id
                        {
                            let body = result.text.trim().to_owned();
                            let body = if body.is_empty() || result.error.is_some() {
                                FALLBACK_BODY.to_owned()
                            } else {
                                body
                            };
                            let title = build_title();
                            writer.write_frame(&Frame::Event(summary_text_event_with(
                                &title, &body,
                            )))?;
                            writer.flush()?;
                            spawn_idle_command(&config, &title, &body);
                            idle = None;
                            if input_closed {
                                break;
                            }
                        }
                    }
                    other => tracing::trace!(
                        target: LOG_TARGET,
                        name = %other.name(),
                        "ignoring unhandled event",
                    ),
                }
            }
            Ok(InMsg::EndOfStream) => {
                input_closed = true;
                if idle.is_none() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => match idle.take() {
                Some(IdleState::WaitingIdle { .. }) => {
                    let query_id = format!("idle-{next_query_id}");
                    next_query_id += 1;
                    tracing::info!(
                        target: LOG_TARGET,
                        query_id = %query_id,
                        "idle deadline elapsed, requesting agent summary",
                    );
                    writer.write_frame(&Frame::Event(Event::ExtAgentQuery(ExtAgentQuery {
                        query_id: query_id.clone(),
                        instruction: SUMMARY_INSTRUCTION.to_owned(),
                        // Notifications doesn't implement a tool —
                        // these fields are only meaningful for the
                        // `delegate` flow.
                        tool_call_id: None,
                        task_name: None,
                    })))?;
                    writer.flush()?;
                    idle = Some(IdleState::WaitingSummary {
                        query_id,
                        deadline: Instant::now() + summary_timeout,
                    });
                }
                Some(IdleState::WaitingSummary { .. }) => {
                    tracing::info!(
                        target: LOG_TARGET,
                        "summary timed out, falling back to static text",
                    );
                    let title = build_title();
                    writer.write_frame(&Frame::Event(summary_text_event_with(
                        &title,
                        FALLBACK_BODY,
                    )))?;
                    writer.flush()?;
                    spawn_idle_command(&config, &title, FALLBACK_BODY);
                    if input_closed {
                        break;
                    }
                }
                None => {
                    // Spurious wake-up; nothing to do.
                }
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                input_closed = true;
                if idle.is_none() {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn sound_event(value: &str) -> Event {
    Event::Osc1337SetUserVar(Osc1337SetUserVar {
        name: SOUND_VAR_NAME.to_owned(),
        value: value.to_owned(),
    })
}

/// True when `event` belongs to a side conversation spawned by an
/// extension (`PromptOriginator::Extension`). Side conversations
/// share the bus with the user's interactive turn; this extension
/// must skip them so sub-agent activity (e.g. a `delegate` sub-task
/// or this extension's own idle-summarizer query) doesn't fire
/// chimes or perturb the idle timer.
fn is_sub_agent_event(event: &Event) -> bool {
    match event {
        Event::AgentPromptSubmitted(s) => !s.originator.is_user(),
        Event::AgentResponseUpdated(u) => !u.originator.is_user(),
        Event::AgentResponseFinished(f) => !f.originator.is_user(),
        Event::UiPromptSubmitted(p) => !p.originator.is_user(),
        Event::SessionPromptCreated(p) => !p.originator.is_user(),
        _ => false,
    }
}

/// Build the OSC `SetUserVar` event whose payload is the JSON
/// schema `user-text-notification.sh` emits. The `app_name` field
/// gives downstream consumers a stable source-app indicator, so we
/// don't need to repeat "tau" inside the title text.
fn summary_text_event_with(title: &str, body: &str) -> Event {
    let payload = serde_json::json!({
        "urgency": "normal",
        "title": title,
        "body": body,
        "app_name": NOTIFY_APP_NAME,
    })
    .to_string();
    Event::Osc1337SetUserVar(Osc1337SetUserVar {
        name: TEXT_VAR_NAME.to_owned(),
        value: payload,
    })
}

/// If the user configured `idle_command`, spawn it detached. Mirrors
/// `user-text-notification.sh`'s calling convention so the script
/// itself (or anything that follows the same shape) can be plugged
/// in directly:
/// - `argv[0]` is the program; the *title* is appended as the next argument.
/// - The body is piped to the command's stdin.
/// - `NOTIFY_URGENCY=normal` and `NOTIFY_APP_NAME=tau` are set in the child's
///   environment.
///
/// Spawned in a worker thread that handles wait/reap; failures log
/// at `warn` and never propagate to the main loop.
fn spawn_idle_command(config: &ExtConfig, title: &str, body: &str) {
    let Some(argv) = config.idle_command.clone() else {
        return;
    };
    if argv.is_empty() {
        tracing::warn!(
            target: LOG_TARGET,
            "idle_command is set but empty; ignoring",
        );
        return;
    }
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || {
        let program = &argv[0];
        let mut command = std::process::Command::new(program);
        command
            .args(&argv[1..])
            .arg(&title)
            .env("NOTIFY_URGENCY", "normal")
            .env("NOTIFY_APP_NAME", NOTIFY_APP_NAME)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    program = %program,
                    error = %e,
                    "idle_command failed to spawn",
                );
                return;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(body.as_bytes());
            // Dropping `stdin` here closes the pipe so the command
            // sees EOF and exits.
        }
        match child.wait() {
            Ok(status) if !status.success() => {
                tracing::warn!(
                    target: LOG_TARGET,
                    program = %program,
                    status = ?status,
                    "idle_command exited non-zero",
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    program = %program,
                    error = %e,
                    "idle_command failed to wait",
                );
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests;
