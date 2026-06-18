//! Harness daemon lifecycle: discovery, spawning, and initial UI wiring.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tau_harness::runtime_dir;

use crate::{CliError, mint_short_id};

/// How this CLI invocation is related to its harness daemon.
///
/// - `Owned`: we spawned the daemon; Drop kills it unless the UI detached
///   (calls [`DaemonHandle::leak`]), in which case we forget the `Child` so the
///   daemon outlives us.
/// - `Attached`: we joined a daemon someone else owns. Drop never touches it.
pub(crate) struct InitialUiStdio {
    pub(crate) stdin: std::process::ChildStdin,
    pub(crate) stdout: std::process::ChildStdout,
}

pub(crate) enum DaemonHandle {
    /// `child` is `Some` until [`leak`] pulls it out.
    Owned {
        child: Option<std::process::Child>,
        harness_path: PathBuf,
        initial_ui: Option<InitialUiStdio>,
    },
    Attached {
        harness_path: PathBuf,
    },
}

impl DaemonHandle {
    pub(crate) fn socket_path(&self) -> PathBuf {
        runtime_dir::socket_path(self.harness_path())
    }

    pub(crate) fn take_initial_ui_stdio(&mut self) -> Option<InitialUiStdio> {
        match self {
            Self::Owned { initial_ui, .. } => initial_ui.take(),
            Self::Attached { .. } => None,
        }
    }

    fn harness_path(&self) -> &Path {
        match self {
            Self::Owned { harness_path, .. } | Self::Attached { harness_path } => harness_path,
        }
    }

    /// Consume the handle without killing the child.
    ///
    /// Used by `/detach`: we want the daemon to outlive this CLI,
    /// whether we spawned it or attached to it. For `Owned` this
    /// `mem::forget`s the `Child` — on Linux its parent becomes init
    /// on our exit, which is exactly what we want for a long-lived
    /// daemon.
    pub(crate) fn leak(mut self) {
        if let Self::Owned { child, .. } = &mut self
            && let Some(child) = child.take()
        {
            std::mem::forget(child);
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Self::Owned {
            child: Some(child), ..
        } = self
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Attached, or Owned-after-leak: do nothing. The daemon keeps
        // running so other UIs can still use it, or this same UI can
        // `tau -a` back in later.
    }
}

/// Resolves a fresh session id for one `tau` invocation.
pub(crate) fn resolve_run_session_id() -> Result<String, CliError> {
    let cwd = std::env::current_dir()?;
    Ok(mint_session_id(&cwd))
}

pub(crate) fn mint_session_id(cwd: &Path) -> String {
    let basename = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    mint_short_id(basename)
}

pub(crate) struct DaemonOutput {
    pub(crate) stderr: Stdio,
}

pub(crate) fn daemon_output_for_session(session_id: &str) -> Result<DaemonOutput, CliError> {
    // Route the daemon's stderr (where its tracing subscriber writes) into the
    // per-session harness log so it sits next to per-extension logs under
    // `<session>/logs/`. The CLI's own tracing still goes to `ui.log`; the two
    // streams are intentionally separated so a session post-mortem doesn't need
    // to pull from two places.
    let sessions_dir = tau_session_inspect::default_sessions_dir();
    let harness_log = tau_harness::harness_log_path(&sessions_dir, session_id);
    if let Some(parent) = harness_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&harness_log)
        .map(Stdio::from)?;
    Ok(DaemonOutput { stderr })
}

pub(crate) struct DaemonCliOverrides<'a> {
    pub(crate) role: &'a [tau_config::settings::RoleCliOverride],
    pub(crate) extension: &'a [tau_config::settings::ExtensionCliOverride],
    pub(crate) harness_config: &'a [tau_config::settings::HarnessConfigCliOverride],
}

pub(crate) fn resolve_daemon(
    attach: bool,
    session_id: &str,
    daemon_output: Option<DaemonOutput>,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
) -> Result<DaemonHandle, CliError> {
    tracing::debug!(target: "tau_cli::startup", attach, session_id, "resolving harness daemon");
    let project_root = std::env::current_dir()?;
    if attach {
        tracing::debug!(target: "tau_cli::startup", project_root = %project_root.display(), "looking for existing harness daemon");
        let harness_path =
            runtime_dir::find_harness_for_dir(&project_root).ok_or(CliError::NoRunningDaemon)?;
        tracing::debug!(target: "tau_cli::startup", harness_path = %harness_path.display(), "attached harness daemon resolved");
        return Ok(DaemonHandle::Attached { harness_path });
    }
    start_daemon(
        session_id,
        daemon_output.expect("daemon output for spawned harness"),
        startup_role,
        cli_overrides,
    )
}

/// Spawns a new harness daemon.
///
/// Child stdin/stdout are reserved for the initial UI protocol and returned
/// immediately; the harness delays extension startup internally until that UI
/// sends its subscribe message.
fn start_daemon(
    session_id: &str,
    output: DaemonOutput,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
) -> Result<DaemonHandle, CliError> {
    let tau_binary = std::env::current_exe()?;
    tracing::debug!(target: "tau_cli::startup", tau_binary = %tau_binary.display(), session_id, "spawning harness daemon");

    let spawn_result = build_daemon_command(DaemonCommandSpec {
        tau_binary: &tau_binary,
        session_id,
        stdout: Stdio::piped(),
        stderr: output.stderr,
        stdin: Stdio::piped(),
        startup_role,
        cli_overrides,
    })
    .spawn();

    let mut child = spawn_result?;

    tracing::debug!(target: "tau_cli::startup", pid = child.id(), "harness daemon spawned");
    let harness_path = runtime_dir::harnesses_dir().join(child.id().to_string());
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| CliError::Participant("missing harness stdin pipe".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::Participant("missing harness stdout pipe".to_owned()))?;
    Ok(DaemonHandle::Owned {
        child: Some(child),
        harness_path,
        initial_ui: Some(InitialUiStdio { stdin, stdout }),
    })
}

struct DaemonCommandSpec<'a> {
    tau_binary: &'a Path,
    session_id: &'a str,
    stdout: Stdio,
    stderr: Stdio,
    stdin: Stdio,
    startup_role: Option<&'a str>,
    cli_overrides: DaemonCliOverrides<'a>,
}

/// Build the `tau component harness` command, reserving stdio for the initial
/// UI protocol.
fn build_daemon_command(spec: DaemonCommandSpec<'_>) -> Command {
    let mut cmd = Command::new(spec.tau_binary);
    cmd.arg("component")
        .arg("harness")
        .env("TAU_SESSION_ID", spec.session_id)
        // TAU_VERSION/TAU_BUILD/TAU_LAST_MODIFIED used to be forwarded
        // here; the harness child now reads its own `built` snapshot
        // (see `tau_harness::version::export_to_env`) and publishes
        // them to its own environment instead.
        // Default-enable info logging in the child process so `tau`
        // captures harness logs without requiring an env var. Users
        // can still override/filter with `TAU_LOG`.
        .env(
            "TAU_LOG",
            std::env::var("TAU_LOG").unwrap_or_else(|_| {
                "tau_harness=info,tau_cli=info,provider-builtin=info".to_owned()
            }),
        )
        .stdin(spec.stdin)
        .stdout(spec.stdout)
        .stderr(spec.stderr);

    for key in [
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDS_FIRST_FD",
        "LISTEN_FDNAMES",
    ] {
        cmd.env_remove(key);
    }

    if let Some(role) = spec.startup_role.filter(|role| !role.is_empty()) {
        cmd.env(tau_harness::STARTUP_ROLE_ENV, role);
    }
    if !spec.cli_overrides.role.is_empty() {
        cmd.env(
            tau_harness::ROLE_CLI_OVERRIDES_ENV,
            serde_json::to_string(spec.cli_overrides.role).expect("role overrides serialize"),
        );
    }
    if !spec.cli_overrides.extension.is_empty() {
        cmd.env(
            tau_harness::EXTENSION_CLI_OVERRIDES_ENV,
            serde_json::to_string(spec.cli_overrides.extension)
                .expect("extension overrides serialize"),
        );
    }
    if !spec.cli_overrides.harness_config.is_empty() {
        cmd.env(
            tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV,
            serde_json::to_string(spec.cli_overrides.harness_config)
                .expect("harness config overrides serialize"),
        );
    } else {
        cmd.env_remove(tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV);
    }

    cmd.arg("--initial-ui-stdio");

    cmd
}

#[cfg(test)]
mod tests;
