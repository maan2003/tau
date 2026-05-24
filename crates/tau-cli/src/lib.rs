//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.

pub mod cli;

mod action_commands;
mod chat;
mod daemon;
mod event_renderer;
mod print_prompt;
mod prompt_history;
mod send;
mod settings_registry;
mod theme;
mod tool_render;
mod ui_logging;

use std::sync::{Mutex, MutexGuard};
use std::{fmt, io};

use tau_harness::{SessionLaunchStatus, runtime_dir};

use crate::chat::run_chat;
use crate::daemon::resolve_run_session_id;

/// Single shared message for mutex-poison panics: every mutex in this
/// crate is held only for short, infallible critical sections, so poison
/// means another thread panicked mid-update and continuing is unsafe.
pub(crate) const MUTEX_POISONED: &str = "mutex poisoned";

/// Locks `mutex` and panics on poison. Centralizes the panic message so
/// individual call sites read as `let mut g = locked(&m);` instead of
/// repeating `.expect("... mutex poisoned")` everywhere.
pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect(MUTEX_POISONED)
}

mod built_info;

const STARTUP_PUNS: &[&str] = &[
    "Tau is like Pi, but twice as much.",
    "A whole new angle on coding agents.",
    "Tau day is every day if you care about circles enough.",
    "Come for the agent, stay for the circumference discourse.",
    "Tau is the irrational choice for rational Unix hackers.",
    "Small tools, loosely joined — that’s the Tau of Unix.",
    "In Tau, what goes around comes around over stdio.",
    "We’ve come full τurn.",
    "Tau keeps the loop tight and the pipes honest.",
    "Every extension gets its turn in Tau.",
    "Tau speaks fluent stdio with a circular accent.",
    "Agents, tools, sockets, loops: a well-rounded lineup.",
    "Ready, set, Tau!",
    "Tau day to code.",
    "Tau-tau control.",
    "Tau-tally operational.",
    "Tau much power in one terminal.",
    "Tau infinity and beyond.",
    "Tau the line between human and agent.",
    "Tau’s what I’m talking about.",
    "One shell to Tau them all.",
    "Tau-powered, Unix-native.",
    "Complete revolution.",
    "Wrapping around nicely.",
    "Continuous on S¹, probably.",
    "Cohomology remains left as exercise.",
];

pub(crate) fn random_startup_pun() -> &'static str {
    use rand::Rng;
    let idx = rand::thread_rng().gen_range(0..STARTUP_PUNS.len());
    STARTUP_PUNS[idx]
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by the CLI.
#[derive(Debug)]
pub enum CliError {
    Io(io::Error),
    Encode(tau_proto::EncodeError),
    Harness(tau_harness::HarnessError),
    Inspect(tau_session_inspect::InspectError),
    DaemonExited(String),
    NoRunningDaemon,
    Participant(String),
    SessionNotFound(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::Encode(source) => write!(f, "encode error: {source}"),
            Self::Harness(source) => write!(f, "harness error: {source}"),
            Self::Inspect(source) => write!(f, "inspect error: {source}"),
            Self::DaemonExited(msg) => write!(f, "harness daemon exited: {msg}"),
            Self::NoRunningDaemon => f.write_str(
                "no harness daemon running for this project — \
                 drop `--attach` to spawn one",
            ),
            Self::Participant(msg) => write!(f, "participant error: {msg}"),
            Self::SessionNotFound(id) => write!(f, "session not found: `{id}`"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<tau_harness::HarnessError> for CliError {
    fn from(source: tau_harness::HarnessError) -> Self {
        Self::Harness(source)
    }
}

impl From<tau_session_inspect::InspectError> for CliError {
    fn from(source: tau_session_inspect::InspectError) -> Self {
        Self::Inspect(source)
    }
}

// ---------------------------------------------------------------------------
// Build labels and version helpers (shared by chat banner, EventRenderer
// banner, and `tau --version`).
// ---------------------------------------------------------------------------

fn run_harness_component() -> Result<(), Box<dyn std::error::Error>> {
    tau_harness::run_component_with_internal_tools(tau_harness_tools::builtin_handlers())
}

fn build_revision() -> String {
    tau_harness::version::build_revision()
}

fn build_last_modified() -> Option<String> {
    // Fall back to the locally-formatted `built` timestamp when the
    // harness can't produce a date (e.g. a `cargo build` outside of
    // Nix where the date placeholder is unpatched and the harness'
    // `built` snapshot only has the RFC2822 string).
    tau_harness::version::build_last_modified()
        .or_else(|| short_built_time(built_info::BUILT_TIME_UTC))
        .filter(|date| date != "1980-01-01 00:00")
}

fn short_built_time(time: &str) -> Option<String> {
    let input_format = time::macros::format_description!(
        "[weekday repr:short], [day padding:none] [month repr:short] [year] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
    );
    let output_format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    time::OffsetDateTime::parse(time, input_format)
        .ok()?
        .format(output_format)
        .ok()
}

pub(crate) fn build_label_parts() -> (String, String) {
    let version = format!("tau {}", env!("CARGO_PKG_VERSION"));
    let build = match build_last_modified() {
        Some(date) => format!("({}, {})", build_revision(), date),
        None => format!("({})", build_revision()),
    };
    (version, build)
}

fn version_label() -> String {
    let (version, build) = build_label_parts();
    format!("{version} {build}")
}

/// Build the two-line startup banner: logo + name/version/build on the
/// first line, logo continuation + random pun on the second.
pub(crate) fn build_banner(theme: &tau_themes::Theme) -> tau_cli_term::StyledText {
    use tau_themes::names;
    let logo = tau_cli_term::resolve::resolve(theme, names::BANNER_LOGO);
    let name = tau_cli_term::resolve::resolve(theme, names::BANNER_NAME);
    let version_style = tau_cli_term::resolve::resolve(theme, names::BANNER_VERSION);
    let build_style = tau_cli_term::resolve::resolve(theme, names::BANNER_BUILD);
    let pun_style = tau_cli_term::resolve::resolve(theme, names::BANNER_PUN);
    let pun = random_startup_pun();
    let (version, build) = build_label_parts();
    tau_cli_term::StyledText::from(vec![
        tau_cli_term::Span::new("▝▜▛▀ ", logo),
        tau_cli_term::Span::new("tau", name),
        tau_cli_term::Span::new(version.trim_start_matches("tau"), version_style),
        tau_cli_term::Span::new(" ", Default::default()),
        tau_cli_term::Span::new(build, build_style),
        tau_cli_term::Span::new("\n", Default::default()),
        tau_cli_term::Span::new(" ▐▙▖ ", logo),
        tau_cli_term::Span::new(pun, pun_style),
    ])
}

// ---------------------------------------------------------------------------
// Short-id minting (used for both session ids and per-UI log dir ids)
// ---------------------------------------------------------------------------

/// Build an id of the form `<prefix>-<6 base36 chars>`. Used for both
/// session and UI ids so the visual shape is consistent.
pub(crate) fn mint_short_id(prefix: &str) -> String {
    use rand::distributions::Distribution;

    struct Base36;
    impl Distribution<char> for Base36 {
        fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> char {
            let n: u8 = rng.gen_range(0..36);
            if n < 10 {
                (b'0' + n) as char
            } else {
                (b'a' + (n - 10)) as char
            }
        }
    }

    let suffix: String = Base36.sample_iter(rand::thread_rng()).take(6).collect();
    format!("{prefix}-{suffix}")
}

// ---------------------------------------------------------------------------
// `tau init`
// ---------------------------------------------------------------------------

const SAMPLE_CLI: &str = include_str!("../../../config/cli.yaml");
const SAMPLE_HARNESS: &str = include_str!("../../../config/harness.yaml");

fn run_init(force: bool) -> Result<(), CliError> {
    let Some(dir) = tau_config::settings::config_dir() else {
        return Err(CliError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine config directory",
        )));
    };
    std::fs::create_dir_all(&dir)?;

    let files = [("cli.yaml", SAMPLE_CLI), ("harness.yaml", SAMPLE_HARNESS)];

    for (name, content) in &files {
        let path = dir.join(name);
        if path.exists() && !force {
            eprintln!(
                "skip: {} (exists, use --force to overwrite)",
                path.display()
            );
        } else {
            std::fs::write(&path, content)?;
            eprintln!("wrote: {}", path.display());
        }
    }

    eprintln!("next: use `tau provider add` to log in to a hosted LLM provider");

    Ok(())
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

pub type ComponentRunner = fn() -> Result<(), Box<dyn std::error::Error>>;

fn parse_role_cli_overrides<I, S>(args: I) -> Vec<tau_config::settings::RoleCliOverride>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut overrides = Vec::new();
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy();
        if arg == "--" {
            break;
        }
        if arg == "--disable-roles-all" {
            overrides.push(tau_config::settings::RoleCliOverride::DisableAll);
        } else if let Some(role) = arg.strip_prefix("--enable-role=") {
            overrides.push(tau_config::settings::RoleCliOverride::Enable(
                role.to_owned(),
            ));
        } else if arg == "--enable-role" {
            if let Some(role) = args.next() {
                overrides.push(tau_config::settings::RoleCliOverride::Enable(
                    role.to_string_lossy().into_owned(),
                ));
            }
        } else if let Some(role) = arg.strip_prefix("--disable-role=") {
            overrides.push(tau_config::settings::RoleCliOverride::Disable(
                role.to_owned(),
            ));
        } else if arg == "--disable-role"
            && let Some(role) = args.next()
        {
            overrides.push(tau_config::settings::RoleCliOverride::Disable(
                role.to_string_lossy().into_owned(),
            ));
        }
    }
    overrides
}

fn parse_extension_cli_overrides<I, S>(args: I) -> Vec<tau_config::settings::ExtensionCliOverride>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut overrides = Vec::new();
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy();
        if arg == "--" {
            break;
        }
        if arg == "--enable-extensions-all" {
            overrides.push(tau_config::settings::ExtensionCliOverride::EnableAll);
        } else if arg == "--disable-extensions-all" {
            overrides.push(tau_config::settings::ExtensionCliOverride::DisableAll);
        } else if let Some(extension) = arg.strip_prefix("--enable-extension=") {
            overrides.push(tau_config::settings::ExtensionCliOverride::Enable(
                extension.to_owned(),
            ));
        } else if arg == "--enable-extension" {
            if let Some(extension) = args.next() {
                overrides.push(tau_config::settings::ExtensionCliOverride::Enable(
                    extension.to_string_lossy().into_owned(),
                ));
            }
        } else if let Some(extension) = arg.strip_prefix("--disable-extension=") {
            overrides.push(tau_config::settings::ExtensionCliOverride::Disable(
                extension.to_owned(),
            ));
        } else if arg == "--disable-extension"
            && let Some(extension) = args.next()
        {
            overrides.push(tau_config::settings::ExtensionCliOverride::Disable(
                extension.to_string_lossy().into_owned(),
            ));
        }
    }
    overrides
}

/// Describes how an `ext` component gets its global tracing subscriber.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentLogging {
    /// `tau-cli` installs a stderr subscriber before invoking the component.
    CliStderr,
    /// The component installs its own subscriber, or does not emit tracing
    /// logs.
    RunnerManaged,
}

pub struct Component {
    /// Name accepted by the hidden `tau ext <name>` dispatcher.
    pub name: &'static str,
    /// Function that runs the component over stdin/stdout.
    pub runner: ComponentRunner,
    /// Owner of the component's tracing initialization.
    pub logging: ComponentLogging,
}

/// Parses CLI arguments via clap and dispatches to the appropriate
/// command.
pub fn main_with_args() -> std::process::ExitCode {
    main_with_args_and_components(&[])
}

/// Parses CLI arguments via clap and dispatches to the appropriate
/// command, using caller-provided component registrations for hidden
/// `ext`/`component` dispatch.
pub fn main_with_args_and_components(components: &[Component]) -> std::process::ExitCode {
    use std::process::ExitCode;

    use clap::Parser;

    let run = || -> Result<(), CliError> {
        let role_cli_overrides = parse_role_cli_overrides(std::env::args_os());
        let extension_cli_overrides = parse_extension_cli_overrides(std::env::args_os());
        let cli::Cli {
            version,
            role_overrides: _,
            extension_overrides: _,
            run,
            command,
        } = cli::Cli::parse();
        if version {
            println!("{}", version_label());
            return Ok(());
        }

        let command = command.unwrap_or(cli::Command::Run(run));

        match command {
            cli::Command::Run(cli::RunArgs {
                resume,
                config: _config,
                attach,
            }) => {
                let (session_id, session_status) = if attach {
                    let cwd = std::env::current_dir()?;
                    let daemon_dir =
                        runtime_dir::find_harness_for_dir(&cwd).ok_or(CliError::NoRunningDaemon)?;
                    let daemon_session_id =
                        runtime_dir::read_session_id(&daemon_dir).ok_or_else(|| {
                            CliError::Participant(
                                "running daemon did not publish its session id".to_owned(),
                            )
                        })?;
                    if let Some(requested) = resume.as_deref().filter(|s| !s.is_empty())
                        && requested != daemon_session_id
                    {
                        return Err(CliError::Participant(format!(
                            "--attach: daemon is bound to session `{daemon_session_id}`, \
                             cannot resume `{requested}` (start a fresh daemon for that)"
                        )));
                    }
                    (daemon_session_id, SessionLaunchStatus::Resumed)
                } else {
                    resolve_run_session_id(resume.as_deref())?
                };
                run_chat(
                    &session_id,
                    attach,
                    session_status,
                    &role_cli_overrides,
                    &extension_cli_overrides,
                )
            }

            cli::Command::SessionList { sessions_dir } => {
                for line in tau_session_inspect::session_list_lines(sessions_dir)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::SessionShow {
                session_id,
                sessions_dir,
            } => {
                for line in tau_session_inspect::session_lines(sessions_dir, &session_id)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::PolicyShow { state_dir } => {
                for line in tau_session_inspect::policy_lines(state_dir.join("policy.cbor"))? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::Init { force } => run_init(force),

            cli::Command::Provider { args } => tau_ext_provider_builtin::run_provider_cli(&args)
                .map_err(|e| CliError::Participant(e.to_string())),

            cli::Command::Dev { command } => match command {
                cli::DevCommand::Send { session_id, line } => {
                    send::run_send(&session_id, &line.join(" "))
                }
                cli::DevCommand::DumpInitialPrompt { out, message } => {
                    tau_harness::dump_initial_prompt(&out, &message)?;
                    println!("wrote {}", out.display());
                    Ok(())
                }
                cli::DevCommand::PrintPrompt { role } => print_prompt::run_print_prompt(
                    &role,
                    &role_cli_overrides,
                    &extension_cli_overrides,
                ),
            },

            cli::Command::Ext { name } => {
                let built_in_components = [Component {
                    name: "harness",
                    runner: run_harness_component,
                    logging: ComponentLogging::CliStderr,
                }];
                let component = built_in_components
                    .iter()
                    .chain(components)
                    .find(|component| component.name == name)
                    .ok_or_else(|| {
                        let available = built_in_components
                            .iter()
                            .chain(components)
                            .map(|component| component.name)
                            .collect::<Vec<_>>()
                            .join(", ");
                        CliError::Participant(format!(
                            "unknown extension: {name}\navailable: {available}"
                        ))
                    })?;
                match component.logging {
                    ComponentLogging::CliStderr => ui_logging::init_stderr_from_env(
                        "tau_harness=info,tau_cli=info,provider-builtin=info",
                    ),
                    ComponentLogging::RunnerManaged => {}
                }
                (component.runner)().map_err(|e| CliError::Participant(e.to_string()))
            }
        }
    };

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
