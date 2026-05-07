use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const UI_LOG_ENV: &str = "TAU_LOG";
const DEFAULT_FILTER: &str = "tau_cli=info";

/// Initialize stderr tracing for component subcommands that do not
/// have their own logging setup. Uses `TAU_LOG` so startup can be
/// traced across the parent CLI and harness child with one knob.
pub fn init_stderr_from_env(default_filter: &str) {
    let filter = EnvFilter::try_from_env(UI_LOG_ENV)
        .or_else(|_| EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// File-backed tracing writer for one terminal UI instance.
#[derive(Clone)]
struct UiLogWriter {
    path: PathBuf,
}

impl<'a> MakeWriter<'a> for UiLogWriter {
    type Writer = Box<dyn Write + Send + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => Box::new(file),
            Err(_) => Box::new(io::sink()),
        }
    }
}

/// Metadata for the current terminal UI instance log.
pub struct UiLogging {
    ui_id: String,
    dir: PathBuf,
    log_path: PathBuf,
}

impl UiLogging {
    #[must_use]
    pub fn ui_id(&self) -> &str {
        &self.ui_id
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }
}

/// Initialize tracing for this CLI terminal UI instance.
///
/// Logs go to `$XDG_STATE_HOME/tau/uis/<ui-id>/ui.log` (normally
/// `~/.local/state/tau/uis/<ui-id>/ui.log`). The filter comes from
/// `TAU_LOG`, defaulting to `tau_cli=info`.
pub fn init(state_dir: &Path) -> io::Result<UiLogging> {
    let ui_id = mint_ui_id();
    let dir = state_dir.join("uis").join(&ui_id);
    std::fs::create_dir_all(&dir)?;

    let log_path = dir.join("ui.log");
    let mut file = File::create(&log_path)?;
    writeln!(file, "# tau ui log")?;
    writeln!(file, "ui_id={ui_id}")?;
    writeln!(file, "pid={}", std::process::id())?;
    if let Ok(cwd) = std::env::current_dir() {
        writeln!(file, "cwd={}", cwd.display())?;
    }
    writeln!(file)?;

    let filter = EnvFilter::try_from_env(UI_LOG_ENV)
        .or_else(|_| EnvFilter::try_new(DEFAULT_FILTER))
        .map_err(io::Error::other)?;
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(UiLogWriter {
            path: log_path.clone(),
        })
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    Ok(UiLogging {
        ui_id,
        dir,
        log_path,
    })
}

fn mint_ui_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut suffix = String::with_capacity(6);
    for _ in 0..6 {
        use rand::Rng;
        let n: u8 = rand::thread_rng().gen_range(0..36);
        if n < 10 {
            suffix.push((b'0' + n) as char);
        } else {
            suffix.push((b'a' + (n - 10)) as char);
        }
    }
    format!("ui-{millis}-{suffix}")
}
