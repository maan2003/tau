//! User settings loaded from `~/.config/tau/` with `.d/` directory
//! overrides. Primary config files:
//!
//! - `cli.ncl` — CLI display preferences
//! - `harness.ncl` — harness settings, extensions, and roles
//!
//! Uses Nickel for human-authored layered configuration.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tau_proto::{CborValue, ModelId, ToolName};

// ---------------------------------------------------------------------------
// Built-in configs
//
// Tau ships its baseline `cli.ncl` and `harness.ncl` as ordinary source files
// under `crates/tau-config/config/`, embedded via `include_str!`. The harness
// baseline is wrapped with a sibling contracts file at load time, keeping the
// main defaults close to user-authored Nickel while retaining schema/default
// metadata. Built-ins are layered underneath the user's own files at load time
// (see `load_nickel_layered_with_builtin`) so user partial overrides keep
// working without the public `CliSettings` / `HarnessSettings` types having to
// carry a `#[serde(default)]` and a synthesized `Default` impl that secretly
// parses a file. CLI bindings are defaulted per chord, not per field inside a
// binding record.
// ---------------------------------------------------------------------------

const BUILT_IN_CLI_NCL: &str = include_str!("../config/built-in.cli.ncl");
const BUILT_IN_HARNESS_CONTRACTS_NCL: &str =
    include_str!("../config/built-in.harness.contracts.ncl");
const BUILT_IN_HARNESS_NCL: &str = include_str!("../config/built-in.harness.ncl");

fn built_in_harness_ncl() -> String {
    format!(
        "let harnessContracts = ({}) in ({})",
        BUILT_IN_HARNESS_CONTRACTS_NCL, BUILT_IN_HARNESS_NCL
    )
}

fn parse_built_in<T: for<'de> Deserialize<'de>>(name: &str, text: &str) -> T {
    eval_nickel_to(name, text).unwrap_or_else(|err| {
        panic!("tau ships with malformed {name}: {err}\nthis is a bug; please report it")
    })
}

// ---------------------------------------------------------------------------
// CLI settings
// ---------------------------------------------------------------------------

/// CLI display settings loaded from `cli.ncl`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.cli.ncl` and is layered in by the loader. Use
/// [`CliSettings::built_in`] when you need a fresh, populated value
/// in a test or fallback.
#[derive(Clone, Debug, Deserialize)]
pub struct CliSettings {
    /// Show a greeting message on startup.
    pub greeting: bool,
    /// Show the tau ASCII logo on startup.
    pub show_logo: bool,
    /// Use a bar-shaped cursor in the CLI. When false, use a steady
    /// block cursor instead.
    pub bar_cursor: bool,
    /// Symbol shown before the input prompt.
    pub prompt_symbol: String,
    /// Symbol shown before submitted prompts in the transcript.
    pub submitted_prompt_symbol: String,
    /// Key bindings for prompt-local shell actions. Defaults to an empty map
    /// at the serde layer; the loader merges `built-in.cli.ncl` underneath the
    /// user's bindings.
    #[serde(default)]
    pub bind: HashMap<String, CliBindingAction>,
}

impl CliSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.cli.ncl`.
    pub fn built_in() -> Self {
        parse_built_in("built-in.cli.ncl", BUILT_IN_CLI_NCL)
    }
}

/// Shell command configured for a CLI key binding.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum CliShellCommand {
    Command(String),
    Options { command: String, trim: bool },
}

impl CliShellCommand {
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self::Options {
            command: command.into(),
            trim: false,
        }
    }

    #[must_use]
    pub fn new_trimmed(command: impl Into<String>) -> Self {
        Self::Options {
            command: command.into(),
            trim: true,
        }
    }

    #[must_use]
    pub fn command(&self) -> &str {
        match self {
            Self::Command(command) | Self::Options { command, .. } => command,
        }
    }

    #[must_use]
    pub fn trim(&self) -> bool {
        match self {
            Self::Command(_) => false,
            Self::Options { trim, .. } => *trim,
        }
    }
}

/// CLI key binding action.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CliBindingAction {
    /// Action name, e.g. `shell-prompt-insert`, `shell-prompt-edit`,
    /// `fast-toggle`, or `role-cycle`.
    pub action: String,
    /// Shell command to execute. `None` for actions that don't shell
    /// out (e.g. `prompt-previous`, `prompt-next`, `fast-toggle`,
    /// `role-cycle`).
    pub command: Option<String>,
    /// Whether to trim command stdout before insertion.
    pub trim: bool,
}

impl Default for CliBindingAction {
    fn default() -> Self {
        Self {
            action: "shell-prompt-insert".to_owned(),
            command: None,
            trim: false,
        }
    }
}

// ---------------------------------------------------------------------------
// CLI runtime state
// ---------------------------------------------------------------------------

/// Mutable CLI state persisted across runs at
/// `<state_dir>/cli.json`. Distinct from `CliSettings` (config) —
/// this file is written by the CLI itself in response to
/// `/set <name> <value>` commands.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CliState {
    /// Whether to render file-mutation diffs in their full expanded
    /// form (vs the compact `+N/-M` chip). Controlled by
    /// `/set show-diff <true|false>`.
    pub show_diff: bool,
    /// Whether to render the agent's reasoning summary (the
    /// `agent.thinking` block). Controlled by
    /// `/set show-thinking <true|false>`.
    pub show_thinking: bool,
    /// Whether to render per-turn token usage stats below agent
    /// responses. Controlled by `/set show-token-stats <true|false>`.
    pub show_token_stats: bool,
    /// Whether to render the full-redraw debug counter in the model
    /// status bar. Controlled by `/set redraw-counter <true|false>`.
    pub redraw_counter: bool,
    /// How tool calls are rendered in the transcript. Controlled by
    /// `/set show-tools <off|summarize-turn|summarize-prompt|compact|full>`.
    pub show_tools: ShowTools,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ShowTools {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "summarize-turn")]
    SummarizeTurn,
    #[serde(rename = "summarize-prompt")]
    SummarizePrompt,
    #[serde(rename = "compact")]
    Compact,
    #[serde(rename = "full", alias = "on")]
    #[default]
    Full,
}

impl ShowTools {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::SummarizeTurn => "summarize-turn",
            Self::SummarizePrompt => "summarize-prompt",
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "summarize-turn" => Some(Self::SummarizeTurn),
            "summarize-prompt" => Some(Self::SummarizePrompt),
            "compact" => Some(Self::Compact),
            "full" | "on" => Some(Self::Full),
            _ => None,
        }
    }
}

impl Default for CliState {
    fn default() -> Self {
        Self {
            show_diff: false,
            show_thinking: true,
            show_token_stats: false,
            redraw_counter: false,
            show_tools: ShowTools::Full,
        }
    }
}

impl CliState {
    /// Load the persisted CLI state. Missing / malformed file → defaults.
    #[must_use]
    pub fn load(dirs: &TauDirs) -> Self {
        let Some(dir) = dirs.state_dir.as_ref() else {
            return Self::default();
        };
        let path = dir.join("cli.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Persist current state. Best-effort: a slash command never fails
    /// because the user's state dir is read-only, but failures are
    /// logged on stderr so a silently-resetting state dir is visible
    /// to the user.
    pub fn save(&self, dirs: &TauDirs) {
        let Some(dir) = dirs.state_dir.as_ref() else {
            return;
        };
        if let Err(error) = self.save_inner(dir) {
            eprintln!(
                "tau: failed to persist CLI state to {}: {error}",
                dir.join("cli.json").display()
            );
        }
    }

    fn save_inner(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("cli.json");
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

// ---------------------------------------------------------------------------
// Harness settings
// ---------------------------------------------------------------------------

/// One named tools-profile: tool name -> enabled/disabled override.
pub type ToolsProfile = HashMap<ToolName, bool>;
/// All named tools-profiles loaded from `harness.ncl`.
pub type ToolsProfiles = HashMap<String, ToolsProfile>;

/// Harness/agent settings loaded from `harness.ncl`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.harness.ncl` and is layered in by the loader.
/// Use [`HarnessSettings::built_in`] when you need a fresh,
/// populated value in a test or fallback.
#[derive(Clone, Debug)]
pub struct HarnessSettings {
    /// Number of days to keep inactive session state directories.
    /// Set to `0` to disable session cleanup.
    pub session_retention_days: u64,

    /// Extension table, keyed by name. Built-in entries (`provider-openai`,
    /// `core-shell`, etc.) live in `config/built-in.harness.ncl`;
    /// anything the user writes here overrides those per-field, or adds
    /// a new extension.
    ///
    /// Example `harness.ncl`:
    /// ```nickel
    /// {
    ///   extensions = {
    ///     // disable the built-in shell extension
    ///     "core-shell" = { enable = false },
    ///     // run the OpenAI provider through ssh on a remote box
    ///     "provider-openai" = { prefix = ["ssh", "user@host"] },
    ///     // a third-party extension
    ///     mything = { command = ["/usr/local/bin/my-tau-ext"] },
    ///   },
    /// }
    /// ```
    pub extensions: HashMap<String, ExtensionEntry>,

    /// Harness-owned role defaults. Each role is a partial set of model
    /// settings; missing fields use provider/model fallbacks for the selected
    /// provider-published model.
    pub roles: HashMap<String, AgentRole>,

    /// Named per-tool enablement overlays keyed by tool name. Each
    /// role may opt into one profile via `toolsProfile`; profile
    /// entries override an extension tool's `enabled_by_default` hint.
    pub tools_profiles: ToolsProfiles,
}

/// Harness settings plus the composed Nickel source they were exported from.
///
/// The harness keeps this source around so non-exported Nickel fields, such as
/// lazy system-prompt templates, can be evaluated later with runtime context.
#[derive(Clone, Debug)]
pub struct LoadedHarnessSettings {
    /// Exported harness settings consumed by Rust.
    pub settings: HarnessSettings,
    /// The exact built-in/user/drop-in Nickel layer expression used to export
    /// [`Self::settings`].
    pub nickel_source: String,
}

#[derive(Deserialize)]
struct HarnessSettingsWire {
    session_retention_days: u64,
    extensions: HashMap<String, ExtensionEntry>,
    #[serde(default)]
    roles: HashMap<String, AgentRole>,
    #[serde(default, rename = "defaultRoles")]
    default_roles: HashMap<String, AgentRole>,
    #[serde(default, rename = "toolsProfiles")]
    tools_profiles: ToolsProfiles,
}

impl<'de> Deserialize<'de> for HarnessSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HarnessSettingsWire::deserialize(deserializer)?;
        let mut roles = wire.roles;
        for (name, legacy_role) in wire.default_roles {
            roles
                .entry(name)
                .and_modify(|role| role.apply_overrides_from(&legacy_role))
                .or_insert(legacy_role);
        }
        Ok(Self {
            session_retention_days: wire.session_retention_days,
            extensions: wire.extensions,
            roles,
            tools_profiles: wire.tools_profiles,
        })
    }
}

impl HarnessSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.harness.ncl`.
    pub fn built_in() -> Self {
        parse_built_in("built-in.harness.ncl", &built_in_harness_ncl())
    }

    #[must_use]
    pub fn session_retention(&self) -> Option<Duration> {
        if self.session_retention_days == 0 {
            return None;
        }
        Some(Duration::from_secs(
            self.session_retention_days.saturating_mul(24 * 60 * 60),
        ))
    }
}

/// One entry in the harness's `extensions` map.
///
/// All fields are optional on the wire so users can override just the
/// fields they care about for built-in extensions. Nickel layering merges
/// user config with built-in defaults before this is deserialized. `None`
/// on any field means neither built-in nor user config set that field.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionEntry {
    /// argv prefix prepended before `command`. Useful for wrappers
    /// that don't change the inner command, e.g.
    /// `["ssh", "user@host"]` to run remotely or
    /// `["bwrap", "--ro-bind", "/", "/", "--"]` to sandbox.
    pub prefix: Option<Vec<String>>,

    /// argv of the extension itself. `command[0]` is the executable;
    /// the rest are arguments. Entries can omit `command` and use a
    /// non-empty `suffix` to piggyback on the running tau binary.
    pub command: Option<Vec<String>>,

    /// argv suffix appended after the current tau executable when `command` is
    /// absent. Built-in extensions use this to select in-binary extension
    /// subcommands without hard-coding an install path in Nickel. If `command`
    /// is present, the resolver treats that argv as complete and ignores
    /// `suffix`; include any extra arguments directly in `command`.
    pub suffix: Option<Vec<String>>,

    /// Whether to run this extension. Defaults to the built-in's `enable`
    /// when present, otherwise the harness treats absence as `true`. Set to
    /// `false` to keep the entry in config but skip spawning.
    pub enable: Option<bool>,

    /// Role tag. Built-in providers use `role: "provider"`; entries
    /// without that role are treated as tool extensions.
    pub role: Option<String>,

    /// Free-form CBOR-compatible configuration object handed to the extension
    /// at startup via `LifecycleConfigure`. The harness does not
    /// interpret it — the extension defines and validates its own
    /// schema. Absent after Nickel layering means the harness uses an empty
    /// config object.
    #[serde(deserialize_with = "deserialize_extension_config_opt")]
    pub config: Option<CborValue>,
}

// ---------------------------------------------------------------------------
// Harness roles
// ---------------------------------------------------------------------------

/// Partial harness role settings loaded from `harness.ncl` and persisted
/// to state. `None` means "use the selected model's fallback" for every field.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentRole {
    /// Short free-form summary shown in role-selection completion menus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Model id preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Reasoning effort preferred by this role.
    #[serde(deserialize_with = "deserialize_opt_from_json_string")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<tau_proto::Effort>,
    /// Output verbosity preferred by this role.
    #[serde(deserialize_with = "deserialize_opt_from_json_string")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<tau_proto::Verbosity>,
    /// Thinking-summary mode preferred by this role.
    #[serde(deserialize_with = "deserialize_opt_from_json_string")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingSummary")]
    pub thinking_summary: Option<tau_proto::ThinkingSummary>,
    /// Provider service tier preferred by this role.
    #[serde(deserialize_with = "deserialize_opt_from_json_string")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "serviceTier")]
    pub service_tier: Option<tau_proto::ServiceTier>,
    /// Name of the harness tools profile applied when this role is active.
    #[serde(skip_serializing_if = "Option::is_none", rename = "toolsProfile")]
    pub tools_profile: Option<String>,
}

impl AgentRole {
    fn apply_overrides_from(&mut self, override_role: &Self) {
        if let Some(description) = &override_role.description {
            self.description = Some(description.clone());
        }
        if let Some(model) = &override_role.model {
            self.model = Some(model.clone());
        }
        if let Some(effort) = override_role.effort {
            self.effort = Some(effort);
        }
        if let Some(verbosity) = override_role.verbosity {
            self.verbosity = Some(verbosity);
        }
        if let Some(thinking_summary) = override_role.thinking_summary {
            self.thinking_summary = Some(thinking_summary);
        }
        if let Some(service_tier) = override_role.service_tier {
            self.service_tier = Some(service_tier);
        }
        if let Some(tools_profile) = &override_role.tools_profile {
            self.tools_profile = Some(tools_profile.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Errors from settings loading.
#[derive(Debug)]
pub enum SettingsError {
    /// Reading a configuration file failed.
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Nickel parsing/evaluation failed.
    Nickel(String),
    /// Exporting Nickel data into the Rust settings schema failed.
    Deserialize(String),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Nickel(message) | Self::Deserialize(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Nickel(_) | Self::Deserialize(_) => None,
        }
    }
}

/// Returns the default tau config directory (`~/.config/tau`).
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tau"))
}

/// Returns the default tau state directory (`~/.local/state/tau`).
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("tau"))
}

/// Returns the per-session storage root inside `state_dir`. Each
/// session lives in its own directory at
/// `<state_dir>/sessions/<session_id>/`; grouping them under a
/// dedicated subdirectory keeps the state dir's top level reserved
/// for tau-wide scalar state (`policy.cbor`, `cli.json`, …).
#[must_use]
pub fn sessions_dir_of(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

/// Returns the default tau per-session storage root
/// (`~/.local/state/tau/sessions`).
#[must_use]
pub fn sessions_dir() -> Option<PathBuf> {
    state_dir().map(|d| sessions_dir_of(&d))
}

/// Overridable directory layout for tau. Use the defaults (`Self::default()`)
/// for normal user runs or construct explicit paths for tests and custom
/// installations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TauDirs {
    /// Where to look for `cli.ncl`, `harness.ncl`, etc.
    pub config_dir: Option<PathBuf>,
    /// Where to read/write runtime state like `harness.json5`.
    pub state_dir: Option<PathBuf>,
}

impl Default for TauDirs {
    fn default() -> Self {
        Self {
            config_dir: config_dir(),
            state_dir: state_dir(),
        }
    }
}

/// Loads CLI settings from `cli.ncl` with `cli.d/*.ncl` overrides.
pub fn load_cli_settings() -> Result<CliSettings, SettingsError> {
    load_cli_settings_in(&TauDirs::default())
}

/// Like [`load_cli_settings`] but reads from an explicit directory layout.
///
/// The embedded `built-in.cli.ncl` is layered underneath the user's
/// own `cli.ncl` (and any `cli.d/*.ncl` drop-ins), so the user
/// can write a partial file and unmentioned fields fall back to the
/// shipped defaults. The `bind` map is merged per-key on top so a
/// user customizing one chord doesn't lose the others. Binding records
/// themselves are replaced as a unit; built-in binding fields are not
/// merged into user-provided binding records.
pub fn load_cli_settings_in(dirs: &TauDirs) -> Result<CliSettings, SettingsError> {
    load_nickel_layered_with_builtin(
        BUILT_IN_CLI_NCL.to_owned(),
        dirs.config_dir.as_deref(),
        "cli",
    )
}

/// Loads harness settings from `harness.ncl` with `harness.d/*.ncl`
/// overrides.
pub fn load_harness_settings() -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_in(&TauDirs::default())
}

/// Loads harness settings and retains the composed Nickel program used to
/// produce them.
pub fn load_harness_settings_with_source() -> Result<LoadedHarnessSettings, SettingsError> {
    load_harness_settings_with_source_in(&TauDirs::default())
}

/// Like [`load_harness_settings`] but reads from an explicit directory layout.
pub fn load_harness_settings_in(dirs: &TauDirs) -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_with_source_in(dirs).map(|loaded| loaded.settings)
}

/// Like [`load_harness_settings_with_source`] but reads from an explicit
/// directory layout.
pub fn load_harness_settings_with_source_in(
    dirs: &TauDirs,
) -> Result<LoadedHarnessSettings, SettingsError> {
    let nickel_source = composed_nickel_source_with_builtin(
        built_in_harness_ncl(),
        dirs.config_dir.as_deref(),
        "harness",
    )?;
    let settings = eval_nickel_to("composed harness.ncl", &nickel_source)?;
    Ok(LoadedHarnessSettings {
        settings,
        nickel_source,
    })
}

/// Stacks an embedded built-in Nickel string underneath the user's files.
/// `T` therefore doesn't need a `Default` impl — the built-in layer always
/// supplies every required field.
fn load_nickel_layered_with_builtin<T: for<'de> Deserialize<'de>>(
    built_in_text: String,
    dir: Option<&Path>,
    name: &str,
) -> Result<T, SettingsError> {
    let source = composed_nickel_source_with_builtin(built_in_text, dir, name)?;
    eval_nickel_to(&format!("composed {name}.ncl"), &source)
}

fn composed_nickel_source_with_builtin(
    built_in_text: String,
    dir: Option<&Path>,
    name: &str,
) -> Result<String, SettingsError> {
    let mut layers = vec![format!("({built_in_text})")];

    if let Some(dir) = dir {
        let base_path = dir.join(format!("{name}.ncl"));
        if base_path.exists() {
            layers.push(import_expr(&base_path));
        }

        let drop_dir = dir.join(format!("{name}.d"));
        if drop_dir.is_dir() {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(&drop_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|ext| ext == "ncl"))
                .collect();
            paths.sort();
            for path in paths {
                layers.push(import_expr(&path));
            }
        }
    }

    Ok(layers.join(" & "))
}

fn eval_nickel_to<T: for<'de> Deserialize<'de>>(
    name: &str,
    text: &str,
) -> Result<T, SettingsError> {
    let mut context = nickel_lang::Context::new().with_source_name(name.to_owned());
    let expr = context
        .eval_deep_for_export(text)
        .map_err(format_nickel_error)?;
    expr.to_serde()
        .map_err(|err| SettingsError::Deserialize(err.to_string()))
}

fn import_expr(path: &Path) -> String {
    format!("(import {:?})", path.display().to_string())
}

fn deserialize_opt_from_json_string<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: for<'a> Deserialize<'a>,
{
    let Some(value) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    serde_json::from_value(serde_json::Value::String(value))
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn deserialize_extension_config_opt<'de, D>(deserializer: D) -> Result<Option<CborValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut value = Option::<CborValue>::deserialize(deserializer)?;
    if let Some(value) = value.as_mut() {
        tau_proto::normalize_integral_cbor_floats(value);
    }
    Ok(value)
}

fn format_nickel_error(error: nickel_lang::Error) -> SettingsError {
    let mut out = Vec::new();
    if error
        .format(&mut out, nickel_lang::ErrorFormat::Text)
        .is_ok()
    {
        SettingsError::Nickel(String::from_utf8_lossy(&out).into_owned())
    } else {
        SettingsError::Nickel(format!("{error:?}"))
    }
}

#[cfg(test)]
mod tests;
