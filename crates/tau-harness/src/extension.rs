//! Extension lifecycle tracking and the spawn helpers used to start both
//! supervised child-process and in-process extensions.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use tau_config::ExtensionConfig;
use tau_core::{Connection, ConnectionMetadata, ConnectionOrigin, EventBus};
use tau_proto::ClientKind;

use crate::error::HarnessError;
use crate::event::{
    ChannelSink, HarnessEvent, WriterShutdown, spawn_reader_thread, spawn_writer_thread,
};
use crate::prompt::chrono_free_date;

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
    /// PID of supervised child process, or current process for in-process.
    pub(crate) pid: Option<u32>,
    /// In-process extension thread handle (for join on shutdown).
    pub(crate) in_process_thread: Option<JoinHandle<Result<(), String>>>,
    /// Original config for supervised extensions. Present only for
    /// out-of-process children that the harness can respawn.
    pub(crate) supervised_config: Option<ExtensionConfig>,
    /// Number of restart attempts performed by the harness.
    pub(crate) restart_attempt: u32,
    /// Current lifecycle state. See `extensions_all_ready` for how this
    /// gates dispatch.
    pub(crate) state: ExtensionState,
    /// Highest `LogEventId` the extension has acknowledged. Cumulative —
    /// any id `<= last_acked` is considered processed. Used by future
    /// reconnect/replay machinery; today it's tracked but not yet
    /// consumed.
    pub(crate) last_acked: tau_proto::LogEventId,
}

pub(crate) fn spawn_in_process<F>(
    name: &str,
    kind: ClientKind,
    run: F,
    bus: &mut EventBus,
    tx: &Sender<HarnessEvent>,
) -> Result<(tau_proto::ConnectionId, JoinHandle<Result<(), String>>), HarnessError>
where
    F: FnOnce(UnixStream, UnixStream) -> Result<(), String> + Send + 'static,
{
    // Two unidirectional pairs so dropping one end cleanly EOFs the
    // other — no shared clones keeping the socket alive.
    let (ext_read, harness_write) = UnixStream::pair()?; // harness → extension
    let (harness_read, ext_write) = UnixStream::pair()?; // extension → harness

    let writer_tx = spawn_writer_thread(harness_write, WriterShutdown::CloseStream);
    let conn_id = bus.connect(Connection::new(
        ConnectionMetadata {
            id: tau_proto::ConnectionId::default(),
            name: name.to_owned(),
            kind,
            origin: ConnectionOrigin::Supervised,
        },
        Box::new(ChannelSink { tx: writer_tx }),
    ));

    spawn_reader_thread(conn_id.clone(), harness_read, tx.clone());

    let thread = thread::spawn(move || run(ext_read, ext_write));
    Ok((conn_id, thread))
}

/// Path of the per-session, per-extension stderr log:
/// `<state_dir>/<session_id>/extensions/<name>.log`. Stays inside the
/// session dir so a session is self-contained (logs sit next to
/// `events.jsonl` and the session's `log.cbor`).
pub(crate) fn extension_stderr_log_path(state_dir: &Path, session_id: &str, name: &str) -> PathBuf {
    state_dir
        .join(session_id)
        .join("extensions")
        .join(format!("{name}.log"))
}

pub(crate) fn spawn_supervised(
    config: &ExtensionConfig,
    kind: ClientKind,
    stderr_log_path: Option<PathBuf>,
    bus: &mut EventBus,
    tx: &Sender<HarnessEvent>,
) -> Result<(tau_proto::ConnectionId, u32), HarnessError> {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if stderr_log_path.is_some() {
        command.stderr(Stdio::piped());
    } else {
        command.stderr(Stdio::inherit());
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

    let writer_tx = spawn_writer_thread(stdin, WriterShutdown::KillChild(child));
    let conn_id = bus.connect(Connection::new(
        ConnectionMetadata {
            id: tau_proto::ConnectionId::default(),
            name: config.name.clone(),
            kind,
            origin: ConnectionOrigin::Supervised,
        },
        Box::new(ChannelSink { tx: writer_tx }),
    ));

    spawn_reader_thread(conn_id.clone(), stdout, tx.clone());

    Ok((conn_id, child_pid))
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
        if let Some(parent) = log_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "tau: failed to create extension log dir {}: {e}",
                    parent.display()
                );
                return;
            }
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
