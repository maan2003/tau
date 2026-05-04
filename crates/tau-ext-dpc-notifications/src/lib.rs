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
    ClientKind, Event, EventReader, EventSelector, EventWriter, ExtAgentQuery,
    LifecycleConfigError, LifecycleHello, LifecycleReady, LifecycleSubscribe, Osc1337SetUserVar,
    PROTOCOL_VERSION,
};

/// `tracing` target for events emitted from this extension. Matches
/// the convention described in [`tau_extension`]: a short identifier
/// the user can name in `TAU_EXT_LOG=dpc_notifications=trace`.
pub const LOG_TARGET: &str = "dpc_notifications";

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
were you working on, and what do you need from the user now? Keep it \
under 100 characters. Output only the summary, nothing else.";

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

/// User-supplied configuration for this extension. Mirrors the
/// schema documented next to `DEFAULT_IDLE_SECONDS`.
#[derive(serde::Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Idle window, in seconds.
    idle_seconds: u64,
}

impl Default for ExtConfig {
    fn default() -> Self {
        Self {
            idle_seconds: DEFAULT_IDLE_SECONDS,
        }
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
/// event from the reader thread, or a terminal condition that ends
/// the loop.
enum InMsg {
    Event(Event),
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
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-dpc-notifications".into(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
            EventSelector::Exact(tau_proto::EventName::AGENT_RESPONSE_FINISHED),
            EventSelector::Exact(tau_proto::EventName::UI_PROMPT_SUBMITTED),
            // Trailing-edge debounced typing pings from the UI:
            // bumps the idle deadline so the desktop notification
            // doesn't fire while the user is mid-sentence.
            EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
            EventSelector::Exact(tau_proto::EventName::LIFECYCLE_CONFIGURE),
            EventSelector::Exact(tau_proto::EventName::LIFECYCLE_DISCONNECT),
            // Side-query results come back point-to-point from the
            // harness, but we subscribe defensively so the broadcast
            // form (if it ever appears) also reaches us.
            EventSelector::Exact(tau_proto::EventName::EXTENSION_AGENT_QUERY_RESULT),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("dpc-notifications ready".to_owned()),
    }))?;
    writer.flush()?;

    // Spawn a reader thread so the main loop can wait on either an
    // incoming event or an idle deadline via `recv_timeout`. The
    // reader exits naturally when stdin closes, then the channel
    // disconnects and the main loop sees EndOfStream.
    let (tx, rx) = mpsc::channel::<InMsg>();
    let _reader_handle = thread::spawn(move || {
        let mut reader = EventReader::new(BufReader::new(reader));
        loop {
            match reader.read_event() {
                Ok(Some(event)) => {
                    if tx.send(InMsg::Event(event)).is_err() {
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
            Ok(InMsg::Event(event)) => {
                let (_, inner) = event.peel_log();
                tracing::trace!(target: LOG_TARGET, name = %inner.name(), "event received");
                match inner {
                    Event::LifecycleConfigure(msg) => {
                        match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                            Ok(cfg) => {
                                idle_duration = Duration::from_secs(cfg.idle_seconds);
                                tracing::info!(
                                    target: LOG_TARGET,
                                    idle_seconds = cfg.idle_seconds,
                                    "applied config",
                                );
                            }
                            Err(message) => {
                                tracing::warn!(
                                    target: LOG_TARGET,
                                    error = %message,
                                    "rejecting config",
                                );
                                writer.write_event(&Event::LifecycleConfigError(
                                    LifecycleConfigError {
                                        message: message.clone(),
                                    },
                                ))?;
                                writer.flush()?;
                            }
                        }
                    }
                    Event::AgentPromptSubmitted(submitted) => {
                        // Skip side-conversation prompts (e.g. our
                        // own idle-summarizer query). The agent emits
                        // AgentPromptSubmitted as soon as it accepts
                        // any prompt — clearing idle here would
                        // discard the in-flight `WaitingSummary`
                        // deadline and silently drop the result.
                        if !submitted.originator.is_user() {
                            tracing::trace!(
                                target: LOG_TARGET,
                                "skipping non-user AgentPromptSubmitted",
                            );
                            continue;
                        }
                        idle = None;
                    }
                    Event::UiPromptSubmitted(prompt) => {
                        // Skip side-conversation prompts (e.g. our
                        // own idle-summarizer query). Treating them
                        // as a fresh user turn would clear the
                        // in-flight `WaitingSummary` deadline and
                        // drop the result we're about to receive.
                        if !prompt.originator.is_user() {
                            tracing::trace!(
                                target: LOG_TARGET,
                                "skipping non-user UiPromptSubmitted",
                            );
                            continue;
                        }
                        idle = None;
                        if !waiting_for_final_response {
                            writer.write_event(&sound_event(VALUE_AGENT_START))?;
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
                        // pending tool work and the prompt was the
                        // user's interactive turn (filter side
                        // queries — those are *our own* responses).
                        if !finished.tool_calls.is_empty() {
                            tracing::trace!(
                                target: LOG_TARGET,
                                tool_calls = finished.tool_calls.len(),
                                "skipping mid-turn AgentResponseFinished",
                            );
                            continue;
                        }
                        if !finished.originator.is_user() {
                            tracing::trace!(
                                target: LOG_TARGET,
                                "skipping non-user AgentResponseFinished",
                            );
                            continue;
                        }
                        writer.write_event(&sound_event(VALUE_AGENT_END))?;
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
                            writer.write_event(&summary_text_event(&body))?;
                            writer.flush()?;
                            idle = None;
                            if input_closed {
                                break;
                            }
                        }
                    }
                    Event::LifecycleDisconnect(_) => {
                        tracing::info!(target: LOG_TARGET, "disconnect received, exiting");
                        break;
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
                    writer.write_event(&Event::ExtAgentQuery(ExtAgentQuery {
                        query_id: query_id.clone(),
                        instruction: SUMMARY_INSTRUCTION.to_owned(),
                    }))?;
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
                    writer.write_event(&summary_text_event(FALLBACK_BODY))?;
                    writer.flush()?;
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

fn summary_text_event(body: &str) -> Event {
    // `app_name` matches the schema `user-text-notification.sh`
    // emits — downstream consumers use it as the desktop
    // notification's source-app indicator (libnotify's app_name),
    // so we don't need to re-state "tau" inside the title text.
    let payload = serde_json::json!({
        "urgency": "normal",
        "title": build_title(),
        "body": body,
        "app_name": NOTIFY_APP_NAME,
    })
    .to_string();
    Event::Osc1337SetUserVar(Osc1337SetUserVar {
        name: TEXT_VAR_NAME.to_owned(),
        value: payload,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tau_proto::{
        AgentResponseFinished, Event, EventReader, EventWriter, LifecycleDisconnect,
        UiPromptSubmitted,
    };

    use super::*;

    fn drain_lifecycle<R: std::io::Read>(reader: &mut EventReader<R>) {
        // Hello, Subscribe, Ready.
        for _ in 0..3 {
            reader.read_event().expect("read").expect("lifecycle event");
        }
    }

    #[test]
    fn emits_start_and_end_user_var_in_order() {
        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: "hello".into(),
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: Some("done".into()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        // Explicit disconnect so the loop exits without waiting on
        // the (otherwise long) idle deadline triggered by the
        // `AgentResponseFinished`.
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        let start = reader.read_event().expect("read").expect("start event");
        match start {
            Event::Osc1337SetUserVar(osc) => {
                assert_eq!(osc.name, SOUND_VAR_NAME);
                assert_eq!(osc.value, VALUE_AGENT_START);
            }
            other => panic!("expected Osc1337SetUserVar, got {other:?}"),
        }

        let end = reader.read_event().expect("read").expect("end event");
        match end {
            Event::Osc1337SetUserVar(osc) => {
                assert_eq!(osc.name, SOUND_VAR_NAME);
                assert_eq!(osc.value, VALUE_AGENT_END);
            }
            other => panic!("expected Osc1337SetUserVar, got {other:?}"),
        }
    }

    /// Mid-turn `AgentResponseFinished` events (those carrying
    /// pending tool calls) must NOT trigger the end-of-turn sound.
    /// The agent emits one of those per LLM call when it's looping
    /// through tool use; the *turn* only ends with a final
    /// `AgentResponseFinished` that has empty `tool_calls`.
    #[test]
    fn mid_turn_finish_with_tool_calls_does_not_emit_end_sound() {
        use tau_proto::{AgentToolCall, CborValue, ToolNameMaybe};
        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: "hello".into(),
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        // Mid-turn finish: text=None, tool_calls non-empty. No
        // notification should fire.
        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: None,
                tool_calls: vec![AgentToolCall {
                    id: "call-1".into(),
                    name: ToolNameMaybe::from_raw("shell"),
                    arguments: CborValue::Null,
                }],
                input_tokens: None,
                cached_tokens: None,
                thinking: Some("planning".into()),
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        // We expect the user-submit sound but NO end sound, because
        // the tool-bearing AgentResponseFinished is mid-turn.
        let start = reader.read_event().expect("read").expect("start");
        match start {
            Event::Osc1337SetUserVar(osc) => {
                assert_eq!(osc.value, VALUE_AGENT_START);
            }
            other => panic!("expected start OSC, got {other:?}"),
        }
        let next = reader.read_event().expect("read");
        assert!(
            next.is_none(),
            "no further OSC events expected after mid-turn finish, got {next:?}",
        );
    }

    /// After AgentResponseFinished we should see the end-sound OSC
    /// and then, after the configured idle window expires with no
    /// further input, the text-notification OSC carrying a JSON
    /// payload that mirrors `user-text-notification.sh`.
    /// Idle window elapsing must trigger an `ExtAgentQuery` to the
    /// agent for a one-sentence summary. When no result arrives
    /// within the summary timeout, the extension falls back to the
    /// static [`FALLBACK_BODY`] so the user still gets nudged.
    #[test]
    fn idle_timeout_requests_summary_then_falls_back() {
        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: Some("done".into()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle_and_summary_timeout(
            Cursor::new(input),
            &mut output,
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        // First the end-of-turn sound.
        let end = reader.read_event().expect("read").expect("end event");
        let Event::Osc1337SetUserVar(osc) = end else {
            panic!("expected end sound OSC");
        };
        assert_eq!(osc.name, SOUND_VAR_NAME);
        assert_eq!(osc.value, VALUE_AGENT_END);

        // Then, after the (short) idle window, the side-query for a
        // summary.
        let query = reader.read_event().expect("read").expect("ext-query event");
        let Event::ExtAgentQuery(query) = query else {
            panic!("expected ExtAgentQuery, got {query:?}");
        };
        assert!(
            !query.query_id.is_empty(),
            "extension must mint a non-empty query_id",
        );
        assert!(query.instruction.contains("summarize") || query.instruction.contains("Summarize"));

        // Then, after the (short) summary timeout with no response,
        // the static fallback text notification.
        let fallback = reader.read_event().expect("read").expect("fallback event");
        let Event::Osc1337SetUserVar(osc) = fallback else {
            panic!("expected fallback OSC, got {fallback:?}");
        };
        assert_eq!(osc.name, TEXT_VAR_NAME);
        let payload: serde_json::Value =
            serde_json::from_str(&osc.value).expect("fallback payload is JSON");
        assert_eq!(payload["urgency"], "normal");
        assert_eq!(
            payload["title"]
                .as_str()
                .expect("title is a string")
                .starts_with("Agent idle: "),
            true,
            "title should start with `Agent idle: `, got {:?}",
            payload["title"],
        );
        assert_eq!(payload["body"], FALLBACK_BODY);
        assert_eq!(payload["app_name"], NOTIFY_APP_NAME);
    }

    /// When a matching `ExtAgentQueryResult` arrives before the
    /// summary timeout, the text notification's body must be the
    /// agent's summary text rather than the static fallback.
    ///
    /// Coordinates with the running extension via a UnixStream pair:
    /// the test thread reads each emitted event and only writes the
    /// `ExtAgentQueryResult` *after* observing the `ExtAgentQuery`,
    /// so the result lands while the extension is in the
    /// `WaitingSummary` state (not the earlier `WaitingIdle`).
    #[test]
    fn summary_result_populates_notification_body() {
        use std::os::unix::net::UnixStream;

        let (test_side, ext_side) = UnixStream::pair().expect("pair");
        let ext_reader = ext_side.try_clone().expect("clone");
        let ext_writer = ext_side;
        let handle = thread::spawn(move || {
            run_with_idle_and_summary_timeout(
                ext_reader,
                ext_writer,
                Duration::from_millis(50),
                Duration::from_secs(5),
            )
            .expect("run");
        });

        let test_writer_stream = test_side.try_clone().expect("clone");
        let mut writer = EventWriter::new(test_writer_stream);
        let mut reader = EventReader::new(test_side);

        // Drain the lifecycle handshake.
        for _ in 0..3 {
            reader.read_event().expect("read").expect("lifecycle");
        }

        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: Some("done".into()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer.flush().expect("flush");

        // end-of-turn sound, then the side-query.
        let _end = reader.read_event().expect("read").expect("end");
        let query = reader.read_event().expect("read").expect("query");
        let Event::ExtAgentQuery(query) = query else {
            panic!("expected ExtAgentQuery, got {query:?}");
        };

        writer
            .write_event(&Event::ExtAgentQueryResult(
                tau_proto::ExtAgentQueryResult {
                    query_id: query.query_id.clone(),
                    text: "  refactoring the harness state, awaiting next prompt  ".into(),
                    error: None,
                },
            ))
            .expect("write");
        writer.flush().expect("flush");

        let text = reader.read_event().expect("read").expect("text");
        let Event::Osc1337SetUserVar(osc) = text else {
            panic!("expected populated text OSC, got {text:?}");
        };
        let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("payload is JSON");
        assert_eq!(
            payload["body"], "refactoring the harness state, awaiting next prompt",
            "summary body should be trimmed",
        );

        // Cleanly disconnect so the extension exits.
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");
        drop(writer);
        drop(reader);
        handle.join().expect("ext thread");
    }

    /// Trailing-edge typing pings (`UiPromptDraft`) arriving during
    /// the `WaitingIdle` window must extend the deadline so the
    /// idle notification doesn't fire while the user is still
    /// composing. Without this, a slow typer would get the
    /// "what were you working on?" notification mid-sentence.
    #[test]
    fn prompt_draft_extends_idle_deadline() {
        use std::os::unix::net::UnixStream;

        use tau_proto::UiPromptDraft;

        let (test_side, ext_side) = UnixStream::pair().expect("pair");
        let ext_reader = ext_side.try_clone().expect("clone");
        let ext_writer = ext_side;
        let handle = thread::spawn(move || {
            run_with_idle_and_summary_timeout(
                ext_reader,
                ext_writer,
                Duration::from_millis(200),
                Duration::from_millis(50),
            )
            .expect("run");
        });

        let test_writer_stream = test_side.try_clone().expect("clone");
        let mut writer = EventWriter::new(test_writer_stream);
        let mut reader = EventReader::new(test_side);

        for _ in 0..3 {
            reader.read_event().expect("read").expect("lifecycle");
        }

        // Arm the idle deadline.
        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: Some("done".into()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer.flush().expect("flush");

        // end-of-turn sound.
        let _end = reader.read_event().expect("read").expect("end");

        // Send several drafts ~100ms apart. Each one resets the
        // 200ms idle deadline; if the extension honors them
        // correctly no `ExtAgentQuery` should fire during this
        // window.
        for i in 0..5 {
            writer
                .write_event(&Event::UiPromptDraft(UiPromptDraft {
                    session_id: "s1".into(),
                    text: format!("partial draft {i}"),
                }))
                .expect("write");
            writer.flush().expect("flush");
            thread::sleep(Duration::from_millis(100));
        }

        // Stop typing. The next event the extension emits must be
        // the side-query — and crucially, the elapsed time before
        // it fires must be >= the original 200ms (because we kept
        // resetting the deadline) plus the final ~200ms wait.
        let started = Instant::now();
        let query = reader.read_event().expect("read").expect("query");
        let elapsed = started.elapsed();
        let Event::ExtAgentQuery(_) = query else {
            panic!("expected ExtAgentQuery, got {query:?}");
        };
        // Without the deadline reset, the query would have fired
        // at idle_duration (200ms) into the typing window — i.e.
        // ~300ms before we started timing — so the read here would
        // return ~immediately. With the reset, the most recent
        // draft (sent ~100ms ago) bumped the deadline ~200ms into
        // the future, so the read should block for roughly 100ms.
        // 30ms is a deliberately loose lower bound so CI jitter
        // doesn't flake the test.
        assert!(
            elapsed >= Duration::from_millis(30),
            "ExtAgentQuery fired too soon ({elapsed:?}); idle deadline wasn't reset",
        );

        // Disconnect to let the extension exit.
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");
        drop(writer);
        drop(reader);
        handle.join().expect("ext thread");
    }

    /// `UiPromptDraft` arriving while a side-query summary is
    /// already in flight must NOT cancel it (we don't yet have
    /// prompt cancellation). The summary completes normally and
    /// surfaces as the notification body.
    #[test]
    fn prompt_draft_during_waiting_summary_does_not_cancel() {
        use std::os::unix::net::UnixStream;

        use tau_proto::UiPromptDraft;

        let (test_side, ext_side) = UnixStream::pair().expect("pair");
        let ext_reader = ext_side.try_clone().expect("clone");
        let ext_writer = ext_side;
        let handle = thread::spawn(move || {
            run_with_idle_and_summary_timeout(
                ext_reader,
                ext_writer,
                Duration::from_millis(50),
                Duration::from_secs(5),
            )
            .expect("run");
        });

        let test_writer_stream = test_side.try_clone().expect("clone");
        let mut writer = EventWriter::new(test_writer_stream);
        let mut reader = EventReader::new(test_side);

        for _ in 0..3 {
            reader.read_event().expect("read").expect("lifecycle");
        }

        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: Some("done".into()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let _end = reader.read_event().expect("read").expect("end");
        let query = reader.read_event().expect("read").expect("query");
        let Event::ExtAgentQuery(query) = query else {
            panic!("expected ExtAgentQuery, got {query:?}");
        };

        // User starts typing AFTER we've dispatched the side query.
        // The summary must still be allowed to land.
        writer
            .write_event(&Event::UiPromptDraft(UiPromptDraft {
                session_id: "s1".into(),
                text: "typing while summary is in flight".into(),
            }))
            .expect("write");
        writer.flush().expect("flush");

        // Now deliver the summary result.
        writer
            .write_event(&Event::ExtAgentQueryResult(
                tau_proto::ExtAgentQueryResult {
                    query_id: query.query_id,
                    text: "the model's summary".into(),
                    error: None,
                },
            ))
            .expect("write");
        writer.flush().expect("flush");

        // Notification must use the summary body, not be cancelled.
        let text = reader.read_event().expect("read").expect("text");
        let Event::Osc1337SetUserVar(osc) = text else {
            panic!("expected populated text OSC, got {text:?}");
        };
        let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("payload is JSON");
        assert_eq!(payload["body"], "the model's summary");

        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");
        drop(writer);
        drop(reader);
        handle.join().expect("ext thread");
    }

    /// A bogus `config` value (one that doesn't match `ExtConfig`)
    /// must trigger a `LifecycleConfigError` carrying a human-readable
    /// message, so the harness can surface it to the user.
    #[test]
    fn invalid_config_emits_lifecycle_config_error() {
        use tau_proto::{LifecycleConfigure, LifecycleDisconnect};

        // Build a config CBOR value that doesn't match ExtConfig:
        // an unknown field, which `deny_unknown_fields` rejects.
        let bad_config = tau_proto::json_to_cbor(&serde_json::json!({
            "totally_unknown_field": 7,
        }));

        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::LifecycleConfigure(LifecycleConfigure {
                config: bad_config,
            }))
            .expect("write");
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        let err = reader
            .read_event()
            .expect("read")
            .expect("config error event");
        match err {
            Event::LifecycleConfigError(e) => {
                assert!(!e.message.is_empty(), "config error must carry a message",);
            }
            other => panic!("expected LifecycleConfigError, got {other:?}"),
        }
    }

    /// A user prompt arriving inside the idle window must cancel the
    /// pending text notification — only the end-sound OSC should be
    /// emitted before stdin closes.
    #[test]
    fn user_prompt_during_idle_window_cancels_text_notification() {
        use tau_proto::UiPromptSubmitted;

        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: Some("done".into()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer
            .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: "another question".into(),
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        // Long idle window — if the cancel works, we never wait.
        run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        let end = reader.read_event().expect("read").expect("end event");
        let Event::Osc1337SetUserVar(osc) = end else {
            panic!("expected end sound OSC");
        };
        assert_eq!(osc.value, VALUE_AGENT_END);

        // The follow-up user prompt should emit the user-submit
        // sound and cancel the idle deadline.
        let next = reader
            .read_event()
            .expect("read")
            .expect("user-submit event");
        let Event::Osc1337SetUserVar(osc) = next else {
            panic!("expected user-submit sound OSC");
        };
        assert_eq!(osc.value, VALUE_AGENT_START);

        assert!(reader.read_event().expect("read eof").is_none());
    }

    #[test]
    fn duplicate_ui_prompt_submitted_during_same_turn_emits_one_start_sound() {
        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: "hello".into(),
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer
            .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: "internal replay".into(),
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer
            .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-0".into(),
                text: Some("done".into()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("write");
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        let first = reader.read_event().expect("read").expect("first OSC");
        let Event::Osc1337SetUserVar(osc) = first else {
            panic!("expected first sound OSC");
        };
        assert_eq!(osc.value, VALUE_AGENT_START);

        let second = reader.read_event().expect("read").expect("second OSC");
        let Event::Osc1337SetUserVar(osc) = second else {
            panic!("expected second sound OSC");
        };
        assert_eq!(osc.value, VALUE_AGENT_END);

        assert!(reader.read_event().expect("read eof").is_none());
    }
}
