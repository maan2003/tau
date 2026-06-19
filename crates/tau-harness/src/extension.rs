//! Extension lifecycle tracking and the spawn helpers used to start both
//! supervised child-process and in-process extensions.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use rand::distributions::{Alphanumeric, DistString};
use tau_config::settings::InvalidExtensionName;
use tau_core::ConnectionOrigin;
use tau_proto::{ClientKind, HarnessRunId};

use crate::error::HarnessError;
use crate::event::{
    HarnessEvent, WriterCommand, WriterShutdown, spawn_reader_thread_after_initialized,
    spawn_writer_thread,
};
use crate::prompt::chrono_free_date;
use crate::settings::ExtensionConfig;

/// Lifecycle phase of a configured extension. Drives the
/// `extensions_all_ready()` gate that keeps user prompts queued until
/// every desired extension has finished its handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionState {
    /// Process spawned (or in-process thread started); no
    /// `LifecycleHello` seen yet.
    Spawning,
    /// `LifecycleHello` received; waiting for the extension to finish
    /// announcing tools/skills and emit `LifecycleReady`.
    Handshaking,
    /// `LifecycleReady` received; the extension is fully online.
    Ready,
    /// The connection dropped after at least reaching `Spawning`.
    /// Fresh prompts continue with the remaining live providers.
    Disconnected,
}

pub(crate) struct ExtensionEntry {
    pub(crate) name: String,
    pub(crate) instance_id: tau_proto::ExtensionInstanceId,
    pub(crate) connection_id: tau_proto::ConnectionId,
    pub(crate) kind: ClientKind,
    /// Whether startup requires this extension to initialize successfully.
    pub(crate) require: bool,
    /// Whether an unexpected supervised disconnect may be respawned.
    pub(crate) respawn_allowed: bool,
    /// PID of supervised child process, or current process for in-process.
    pub(crate) pid: Option<u32>,
    /// In-process extension thread handle (for join on shutdown).
    pub(crate) in_process_thread: Option<JoinHandle<Result<(), String>>>,
    /// Original config for supervised extensions. Present only for
    /// out-of-process children that the harness can respawn.
    pub(crate) supervised_config: Option<ExtensionConfig>,
    /// Resolved secret values authorized for this extension. Values must not be
    /// logged.
    pub(crate) secrets: std::collections::BTreeMap<String, tau_proto::SecretValue>,
    /// Number of restart attempts performed by the harness.
    pub(crate) restart_attempt: u32,
    /// Current lifecycle state. See `extensions_all_ready` for how this
    /// gates dispatch.
    pub(crate) state: ExtensionState,
}

/// Private one-shot ack that lets an extension reader start forwarding frames
/// after the harness has installed its bus and lifecycle state.
pub(crate) type ExtensionInitializedAck = Sender<()>;

/// Internal request for the harness loop to install an extension connection.
pub(crate) struct ExtensionConnectCommand {
    /// Lifecycle entry to insert once the bus connection exists.
    pub(crate) entry: ExtensionEntry,
    /// Bus metadata origin to report for the connection.
    pub(crate) origin: ConnectionOrigin,
    /// Writer channel owned by the bus connection sink.
    pub(crate) writer_tx: Sender<WriterCommand>,
    /// Ack that releases the reader after state installation completes.
    pub(crate) initialized_ack: ExtensionInitializedAck,
    /// Previous connection id to replace when this is a supervised respawn.
    pub(crate) replaces: Option<tau_proto::ConnectionId>,
}

/// Result of spawning an in-process extension transport.
#[cfg_attr(not(any(test, feature = "echo-agent")), allow(dead_code))]
pub(crate) struct InProcessSpawn {
    /// Connection id assigned before the reader thread starts.
    pub(crate) connection_id: tau_proto::ConnectionId,
    /// Writer channel to install in the bus from the harness loop.
    pub(crate) writer_tx: Sender<WriterCommand>,
    /// In-process extension thread handle to join during shutdown.
    pub(crate) thread: JoinHandle<Result<(), String>>,
    /// Ack that releases the reader after state installation completes.
    pub(crate) initialized_ack: ExtensionInitializedAck,
}

/// Result of spawning a supervised extension transport.
pub(crate) struct SupervisedSpawn {
    /// Connection id assigned before the reader thread starts.
    pub(crate) connection_id: tau_proto::ConnectionId,
    /// Writer channel to install in the bus from the harness loop.
    pub(crate) writer_tx: Sender<WriterCommand>,
    /// OS process id of the supervised child.
    pub(crate) child_pid: u32,
    /// Ack that releases the reader after state installation completes.
    pub(crate) initialized_ack: ExtensionInitializedAck,
}

static NEXT_EXTENSION_CONNECTION_ID: AtomicU64 = AtomicU64::new(0);

fn next_extension_connection_id() -> tau_proto::ConnectionId {
    let next = NEXT_EXTENSION_CONNECTION_ID.fetch_add(1, Ordering::Relaxed) + 1;
    format!("ext-conn-{next}").into()
}

#[cfg_attr(not(any(test, feature = "echo-agent")), allow(dead_code))]
pub(crate) fn spawn_in_process<F>(
    _name: &str,
    _kind: ClientKind,
    run: F,
    tx: &Sender<HarnessEvent>,
) -> Result<InProcessSpawn, HarnessError>
where
    F: FnOnce(UnixStream, UnixStream) -> Result<(), String> + Send + 'static,
{
    // Two unidirectional pairs so dropping one end cleanly EOFs the
    // other — no shared clones keeping the socket alive.
    let (ext_read, harness_write) = UnixStream::pair()?; // harness → extension
    let (harness_read, ext_write) = UnixStream::pair()?; // extension → harness

    let connection_id = next_extension_connection_id();
    let writer_tx = spawn_writer_thread(harness_write, WriterShutdown::CloseStream);

    let (initialized_tx, initialized_rx) = mpsc::channel();
    spawn_reader_thread_after_initialized(
        connection_id.clone(),
        harness_read,
        tx.clone(),
        initialized_rx,
    );

    let thread = thread::spawn(move || run(ext_read, ext_write));
    Ok(InProcessSpawn {
        connection_id,
        writer_tx,
        thread,
        initialized_ack: initialized_tx,
    })
}

/// Returns a short random id for one harness process/run.
#[must_use]
pub fn new_harness_run_id() -> HarnessRunId {
    let value = Alphanumeric.sample_string(&mut rand::thread_rng(), HarnessRunId::LEN);
    HarnessRunId::parse(value).expect("alphanumeric generator creates valid run ids")
}

/// Returns the state root that contains per-run debug directories.
#[must_use]
pub fn debug_root(state_dir: &Path) -> PathBuf {
    state_dir.join("debug")
}

/// Returns the debug directory for one harness run.
#[must_use]
pub fn debug_dir(state_dir: &Path, run_id: &HarnessRunId) -> PathBuf {
    debug_root(state_dir).join(run_id.as_str())
}

/// Harness log directory under a run debug directory.
/// Holds one stderr log file per spawned extension.
pub fn harness_logs_dir(debug_dir: &Path) -> PathBuf {
    debug_dir.join("logs")
}

/// Path of the per-extension stderr log:
/// `<state-dir>/debug/<run-id>/logs/<name>.log`.
///
/// Extension names come from user-authored config, so validate the name before
/// treating it as a harness-owned path component.
pub(crate) fn extension_stderr_log_path(
    debug_dir: &Path,
    name: &str,
) -> Result<PathBuf, InvalidExtensionName> {
    tau_config::settings::validate_extension_name(name)?;
    Ok(harness_logs_dir(debug_dir).join(format!("{name}.log")))
}

fn supervised_command(config: &ExtensionConfig, pipe_stderr: bool) -> Command {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(cwd) = config.cwd.as_ref() {
        command.current_dir(cwd);
    }
    if pipe_stderr {
        command.stderr(Stdio::piped());
    } else {
        command.stderr(Stdio::inherit());
    }
    command
}

pub(crate) fn spawn_supervised(
    config: &ExtensionConfig,
    _kind: ClientKind,
    stderr_log_path: Option<PathBuf>,
    tx: &Sender<HarnessEvent>,
) -> Result<SupervisedSpawn, HarnessError> {
    let mut command = supervised_command(config, stderr_log_path.is_some());
    for key in std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("TAU_SECRET_"))
    {
        command.env_remove(key);
    }
    let mut child = command.spawn().map_err(HarnessError::Io)?;

    let child_pid = child.id();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Participant("missing stdin".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Participant("missing stdout".to_owned()))?;

    if let (Some(log_path), Some(stderr)) = (stderr_log_path, child.stderr.take()) {
        spawn_extension_stderr_logger(config.name.clone(), stderr, log_path);
    }

    let connection_id = next_extension_connection_id();
    let writer_tx = spawn_writer_thread(stdin, WriterShutdown::KillChild(child));

    let (initialized_tx, initialized_rx) = mpsc::channel();
    spawn_reader_thread_after_initialized(
        connection_id.clone(),
        stdout,
        tx.clone(),
        initialized_rx,
    );

    Ok(SupervisedSpawn {
        connection_id,
        writer_tx,
        child_pid,
        initialized_ack: initialized_tx,
    })
}

/// Read an extension's stderr line-by-line and append each line
/// verbatim to `log_path`. Extensions are expected to use
/// `tau_extension::init_logging` (or any other `tracing`-based
/// formatter), which already emits its own timestamps and levels —
/// adding our own prefix would double up the metadata. The thread
/// exits naturally when stderr closes (i.e. the child exits), so
/// callers don't need to track the join handle.
fn spawn_extension_stderr_logger(
    name: String,
    stderr: std::process::ChildStderr,
    log_path: PathBuf,
) {
    use std::io::{BufReader, Write};
    thread::spawn(move || {
        if let Some(parent) = log_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!(
                "tau: failed to create extension log dir {}: {e}",
                parent.display()
            );
            return;
        }
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "tau: failed to open extension log {}: {e}",
                    log_path.display()
                );
                return;
            }
        };

        let _ = writeln!(
            file,
            "--- {} (pid={}) attached at {} ---",
            name,
            std::process::id(),
            chrono_free_date()
        );
        let _ = file.flush();

        let mut reader = BufReader::new(stderr);
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = file.write_all(&buf[..n]);
                    let _ = file.flush();
                }
                Err(_) => break,
            }
        }
        let _ = writeln!(
            file,
            "--- {} stderr closed at {} ---",
            name,
            chrono_free_date()
        );
        let _ = file.flush();
    });
}

#[cfg(test)]
mod tests;
