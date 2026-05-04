//! Public entry points: blocking `run_*` daemons, the embedded
//! single-message helpers, and the small types passed to/from them.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use tau_config::Config;
use tau_proto::{
    ClientKind, Event, EventSelector, LifecycleDisconnect, LifecycleHello, LifecycleSubscribe,
    PROTOCOL_VERSION, UiPromptSubmitted,
};
use tau_socket::SocketPeer;

use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::format::{format_extension_event, format_tool_progress};
use crate::harness::{Harness, default_agent_runner};
use crate::runtime_dir;
use crate::settings::resolve_config;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Serve-loop options for daemon mode.
#[derive(Clone, Debug, Default, Eq, PartialEq, bon::Builder)]
pub struct ServeOptions {
    /// Hard cap on total served clients before the serve loop exits.
    /// Used mainly in tests to bound a run. `None` = unbounded.
    pub max_clients: Option<usize>,
    /// When set, the daemon exits as soon as the last attached UI
    /// socket disconnects. When clear, the daemon keeps running with
    /// no attached UIs — a later `tau run --attach` can pick up the
    /// session. The `ui.detach_request` event flips this at runtime.
    ///
    /// Default `false`: daemon is long-lived unless explicitly told
    /// otherwise.
    #[builder(default)]
    pub exit_on_disconnect: bool,
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
}

/// One completed user interaction with optional progress updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionOutcome {
    pub lifecycle_messages: Vec<String>,
    pub progress_messages: Vec<String>,
    pub response: String,
}

/// Options for a one-shot embedded run.
#[derive(Clone, Debug, Default, Eq, PartialEq, bon::Builder)]
pub struct EmbeddedOptions {
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
}

pub(crate) fn bind_listener(path: &Path) -> Result<UnixListener, HarnessError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path).map_err(HarnessError::from)
}

/// Runs one embedded interaction and returns progress plus the final
/// agent response.
pub fn run_embedded_message_with_trace(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    run_embedded_message_impl(
        state_dir,
        session_id,
        message,
        default_agent_runner,
        EmbeddedOptions::default(),
    )
}

/// Runs one embedded interaction and returns the final agent response.
pub fn run_embedded_message(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(run_embedded_message_with_trace(state_dir, session_id, message)?.response)
}

/// Like [`run_embedded_message_with_trace`] but lets the caller override
/// directory layout and other options.
pub fn run_embedded_message_with_options(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    options: EmbeddedOptions,
) -> Result<InteractionOutcome, HarnessError> {
    run_embedded_message_impl(
        state_dir,
        session_id,
        message,
        default_agent_runner,
        options,
    )
}

/// Like [`run_embedded_message_with_trace`] but uses the echo agent for
/// testing.
pub fn run_embedded_message_with_echo(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_agent::run_echo(r, w).map_err(|e| e.to_string())
    }
    run_embedded_message_impl(
        state_dir,
        session_id,
        message,
        echo_runner,
        EmbeddedOptions::default(),
    )
}

fn run_embedded_message_impl(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    agent_runner: crate::harness::AgentRunner,
    options: EmbeddedOptions,
) -> Result<InteractionOutcome, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = options.dirs.unwrap_or_default();
    let mut harness = Harness::new_with_agent(state_dir, dirs, agent_runner, true, session_id)?;
    let mut outcome = harness.send_user_message(session_id, message, None)?;
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages.clone();
    Ok(outcome)
}

/// Runs a foreground daemon that accepts socket clients.
///
/// `eager_session_id` is the session the harness pre-warms (AGENTS.md +
/// skill discovery) and where `events.jsonl` lands. Subsequent prompts for
/// other session ids lazy-init.
pub fn run_daemon(
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener = bind_listener(&socket_path)?;
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::new(state_dir, dirs, eager_session_id)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Runs a foreground daemon using extensions from configuration.
pub fn run_daemon_with_config(
    config: &Config,
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener = bind_listener(&socket_path)?;
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::from_config(config, state_dir, dirs, eager_session_id)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Sends one user message to a running daemon and returns progress
/// plus the final response.
pub fn send_daemon_message_with_trace(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    let mut peer = SocketPeer::connect(socket_path)?;
    peer.send(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-cli".into(),
        client_kind: ClientKind::Ui,
    }))?;
    peer.send(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Prefix("agent.".to_owned()),
            EventSelector::Prefix("session.".to_owned()),
            EventSelector::Prefix("tool.".to_owned()),
            EventSelector::Prefix("extension.".to_owned()),
            EventSelector::Prefix("harness.".to_owned()),
        ],
    }))?;
    peer.send(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: session_id.into(),
        text: message.to_owned(),
    }))?;

    let started_at = Instant::now();
    let mut lifecycle_messages = Vec::new();
    let mut progress_messages = Vec::new();
    loop {
        if RESPONSE_TIMEOUT <= started_at.elapsed() {
            return Err(HarnessError::ResponseTimeout);
        }
        if let Some(event) = peer.recv_timeout(RESPONSE_TIMEOUT)? {
            // UI clients don't ack — they just consume the inner event.
            let (_log_id, event) = event.peel_log();
            match event {
                Event::ToolProgress(p) => progress_messages.push(format_tool_progress(&p)),
                Event::HarnessInfo(ref info) => {
                    lifecycle_messages.push(info.message.clone());
                }
                Event::ExtensionStarting(_)
                | Event::ExtensionReady(_)
                | Event::ExtensionExited(_)
                | Event::ExtensionRestarting(_) => {
                    lifecycle_messages.push(format_extension_event(&event));
                }
                Event::AgentResponseFinished(finished) if finished.tool_calls.is_empty() => {
                    peer.send(&Event::LifecycleDisconnect(LifecycleDisconnect {
                        reason: Some("done".to_owned()),
                    }))?;
                    return Ok(InteractionOutcome {
                        lifecycle_messages,
                        progress_messages,
                        response: finished.text.unwrap_or_default(),
                    });
                }
                Event::LifecycleDisconnect(d) => {
                    return Err(HarnessError::Participant(
                        d.reason.unwrap_or_else(|| "daemon disconnected".to_owned()),
                    ));
                }
                _ => {}
            }
        }
    }
}

/// Sends one user message to a running daemon and returns the final
/// response.
pub fn send_daemon_message(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(send_daemon_message_with_trace(socket_path, session_id, message)?.response)
}

/// Runs the harness daemon with runtime directory management.
pub fn run_harness_daemon(
    project_root: &Path,
    config: &Config,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let daemon_dir = runtime_dir::prepare_daemon_dir(project_root)?;
    let listener = bind_listener(&daemon_dir.socket_path())?;

    let state_dir = crate::dirs::default_state_dir();
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::from_config(config, &state_dir, dirs, eager_session_id)?;
    harness.emit_info(&format!(
        "event log: {}",
        state_dir
            .join(eager_session_id)
            .join("events.jsonl")
            .display()
    ));

    // Write marker AFTER extensions are ready.
    daemon_dir.write_marker()?;
    daemon_dir.write_pid()?;
    daemon_dir.write_session_id(eager_session_id)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    daemon_dir.cleanup();
    result
}

/// Entrypoint for `tau ext harness`.
pub fn run_component() -> Result<(), Box<dyn std::error::Error>> {
    let project_root = std::env::current_dir()?;
    let config = resolve_config(None)?;
    // The CLI passes the minted/resumed session id via the harness's
    // SESSION_ID env var when spawning a daemon. Fallback to
    // `default_session_id()` covers a bare `tau ext harness`
    // launched without a CLI in front of it.
    let eager_session_id = std::env::var("TAU_SESSION_ID")
        .unwrap_or_else(|_| crate::dirs::default_session_id().to_owned());
    run_harness_daemon(
        &project_root,
        &config,
        &eager_session_id,
        // Exit once the spawning UI leaves. A UI that wants the
        // daemon to outlive it sends `ui.detach_request`, which
        // flips this to `false` at runtime.
        ServeOptions {
            exit_on_disconnect: true,
            ..Default::default()
        },
    )
    .map_err(Into::into)
}
