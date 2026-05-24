use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tau_session_inspect::{default_session_id, default_sessions_dir, default_state_dir};

#[derive(Parser)]
#[command(
    name = "tau",
    about = "Unix-native LLM agent harness",
    disable_version_flag = true
)]
pub struct Cli {
    /// Print version, build revision, and build date.
    #[arg(short = 'V', long = "version", global = true)]
    pub version: bool,

    #[command(flatten)]
    pub role_overrides: RoleOverrideArgs,

    #[command(flatten)]
    pub extension_overrides: ExtensionOverrideArgs,

    #[command(flatten)]
    pub run: RunArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args)]
pub struct RoleOverrideArgs {
    /// Enable a configured role after all config files are loaded.
    #[arg(long = "enable-role", global = true)]
    pub enable_role: Vec<String>,

    /// Disable a configured role after all config files are loaded.
    #[arg(long = "disable-role", global = true)]
    pub disable_role: Vec<String>,

    /// Disable every configured role before later CLI role overrides.
    #[arg(long = "disable-roles-all", global = true, action = clap::ArgAction::Count)]
    pub disable_roles_all: u8,
}

#[derive(Args)]
pub struct ExtensionOverrideArgs {
    /// Enable every configured extension before later CLI extension overrides.
    #[arg(long = "enable-extensions-all", global = true, action = clap::ArgAction::Count)]
    pub enable_extensions_all: u8,

    /// Disable every configured extension before later CLI extension overrides.
    #[arg(long = "disable-extensions-all", global = true, action = clap::ArgAction::Count)]
    pub disable_extensions_all: u8,

    /// Enable a configured extension after all config files are loaded.
    #[arg(long = "enable-extension", global = true)]
    pub enable_extension: Vec<String>,

    /// Disable a configured extension after all config files are loaded.
    #[arg(long = "disable-extension", global = true)]
    pub disable_extension: Vec<String>,
}

#[derive(Args)]
pub struct RunArgs {
    /// Resume an existing session.
    ///
    /// Bare `-r` resumes the most recent session whose `meta.json.cwd`
    /// matches the current working directory. `-r <id>` resumes that
    /// specific session id. Without `-r`, a fresh session id is minted
    /// (`<basename(cwd)>-<rand6>`).
    #[arg(short = 'r', long = "resume", num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,

    /// Select the startup role for a newly spawned session.
    #[arg(long = "role")]
    pub role: Option<String>,

    /// Path to extension configuration file
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Attach to an existing harness daemon for this project instead of
    /// spawning a new one. Errors if no daemon is running.
    #[arg(short = 'a', long)]
    pub attach: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run an interactive agent session.
    ///
    /// By default, `tau` spawns a new harness daemon and attaches to it
    /// for the duration of the session. Pass `--attach` (or `-a`) to
    /// connect to an already-running daemon for the current project
    /// instead — useful for a second UI, or for reconnecting after
    /// `/detach`.
    #[command(hide = true)]
    Run(RunArgs),

    /// List all sessions
    SessionList {
        /// Path to per-session storage root (`<state-dir>/sessions/`)
        #[arg(long, default_value_os_t = default_sessions_dir())]
        sessions_dir: PathBuf,
    },

    /// Show a single session's history
    SessionShow {
        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to per-session storage root (`<state-dir>/sessions/`)
        #[arg(long, default_value_os_t = default_sessions_dir())]
        sessions_dir: PathBuf,
    },

    /// Show persisted policy approvals
    PolicyShow {
        /// Path to tau state directory (policy.cbor lives inside)
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
    },

    /// Copy sample config files to ~/.config/tau/
    Init {
        /// Overwrite existing config files
        #[arg(long)]
        force: bool,
    },

    /// Manage LLM provider profiles (add, remove, list)
    Provider {
        /// Subcommand and arguments (e.g. `add`, `remove <name>`, `list`)
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// Developer-only commands.
    #[command(hide = true, hide_possible_values = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },

    /// Run an internal extension as a standalone process (used by the
    /// harness to spawn extensions from the unified binary).
    #[command(hide = true, alias = "component")]
    Ext {
        /// Extension name (harness, ext-provider-builtin, ext-shell,
        /// ext-test-dummy, ext-std-notifications, ext-websearch, ext-email)
        name: String,
    },
}

#[derive(Subcommand)]
pub enum DevCommand {
    /// Send one line to a running session.
    Send {
        /// Running session identifier.
        session_id: String,

        /// Line to submit. Slash commands are interpreted like the TUI.
        #[arg(required = true, trailing_var_arg = true)]
        line: Vec<String>,
    },

    /// Dump the initial provider prompt built from local config.
    DumpInitialPrompt {
        /// Output path.
        #[arg(long, default_value = "tmp/initial_prompt.txt")]
        out: PathBuf,

        /// Synthetic first user message.
        #[arg(long, default_value = "hello")]
        message: String,
    },

    /// Print the rendered system prompt for a role.
    PrintPrompt {
        /// Role name to render.
        #[arg(short = 'r', long = "role")]
        role: String,
    },

    /// Print the effective tool definitions for a role.
    PrintTools {
        /// Role name whose tools should be printed.
        #[arg(short = 'r', long = "role")]
        role: String,
    },
}
