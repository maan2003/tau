//! User settings loaded from `~/.config/tau/` with `.d/` directory
//! overrides. Primary config files:
//!
//! - `cli.yaml` — CLI display preferences
//! - `harness.yaml` — harness settings, extensions, and roles
//!
//! Uses the `config` crate for layered JSON5/YAML loading.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::de::{Error as _, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};
use tau_proto::{ModelId, PromptContent, PromptPriority, ToolName};

// ---------------------------------------------------------------------------
// Built-in configs
//
// Tau ships its baseline `cli.yaml`, `cli-bindings.json5` and
// `harness.yaml` as ordinary source files under
// `crates/tau-config/config/`, embedded via `include_str!`. They are layered
// underneath user files, with a small role-merge pass for role metadata whose
// semantics differ from generic YAML array replacement.
// ---------------------------------------------------------------------------

const BUILT_IN_CLI_YAML: &str = include_str!("../config/built-in.cli.yaml");
const BUILT_IN_CLI_BINDINGS_YAML: &str = include_str!("../config/built-in.cli-bindings.yaml");
const BUILT_IN_HARNESS_YAML: &str = include_str!("../config/built-in.harness.yaml");

fn parse_built_in_yaml<T: for<'de> Deserialize<'de>>(name: &str, text: &str) -> T {
    serde_yaml_ng::from_str(text).unwrap_or_else(|err| {
        panic!("tau ships with malformed {name}: {err}\nthis is a bug; please report it")
    })
}

// ---------------------------------------------------------------------------
// CLI settings
// ---------------------------------------------------------------------------

/// CLI display settings loaded from `cli.yaml`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.cli.yaml` and is layered in by the loader. Use
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
    /// Whether to render file-mutation diffs in their full expanded
    /// form by default.
    pub show_diff: bool,
    /// Whether to render the agent's reasoning summary by default.
    pub show_thinking: bool,
    /// Whether to render per-turn token usage stats by default.
    pub show_turn_stats: bool,
    /// Whether to render the full-redraw debug counter in the model
    /// status bar by default.
    pub redraw_counter: bool,
    /// How tool calls are rendered in the transcript by default.
    pub show_tools: ShowTools,
    /// How inter-agent and user-agent messages are rendered in the transcript.
    pub show_messages: ShowMessages,
    /// Which built-in color theme to use for the terminal UI.
    pub theme: CliTheme,
    /// Key bindings for prompt-local actions. Defaults to an
    /// empty map at the serde layer; the loader merges
    /// `built-in.cli-bindings.yaml` underneath the user's bindings.
    #[serde(default)]
    pub bind: HashMap<String, CliBindingAction>,
}

impl CliSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.cli.yaml` plus `built-in.cli-bindings.yaml`.
    pub fn built_in() -> Self {
        let mut s: Self = parse_built_in_yaml("built-in.cli.yaml", BUILT_IN_CLI_YAML);
        s.bind = default_cli_bindings();
        s
    }

    /// Return the default runtime UI state derived from static CLI config.
    #[must_use]
    pub fn default_state(&self) -> CliState {
        CliState {
            show_diff: self.show_diff,
            show_thinking: self.show_thinking,
            show_turn_stats: self.show_turn_stats,
            redraw_counter: self.redraw_counter,
            show_tools: self.show_tools,
            show_messages: self.show_messages,
        }
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
    /// Action name, e.g. `submit-prompt`, `insert-newline`,
    /// `shell-prompt-insert`, `shell-prompt-edit`, `fast-toggle`,
    /// `cycle-role`, or `cycle-role-group`.
    pub action: String,
    /// Shell command to execute. `None` for actions that don't shell
    /// out (e.g. `submit-prompt`, `insert-newline`,
    /// `prompt-previous`, `prompt-next`, `fast-toggle`, `cycle-role`, or
    /// `cycle-role-group`).
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

/// Parse the embedded `built-in.cli-bindings.yaml`. Called from
/// [`CliSettings::built_in`] and from [`load_cli_settings_in`] (the
/// latter overlays user bindings on top of this baseline so users
/// don't lose unmentioned keys when they customize a single chord).
pub(crate) fn default_cli_bindings() -> HashMap<String, CliBindingAction> {
    parse_built_in_yaml("built-in.cli-bindings.yaml", BUILT_IN_CLI_BINDINGS_YAML)
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
    /// responses. Controlled by `/set show-turn-stats <true|false>`.
    pub show_turn_stats: bool,
    /// Whether to render the full-redraw debug counter in the model
    /// status bar. Controlled by `/set redraw-counter <true|false>`.
    pub redraw_counter: bool,
    /// How tool calls are rendered in the transcript. Controlled by
    /// `/set show-tools <off|summarize-turn|summarize-prompt|compact|full>`.
    pub show_tools: ShowTools,
    /// How messages between the user and agents, or between agents, are
    /// rendered in the transcript. Controlled by `/set show-messages <mode>`.
    pub show_messages: ShowMessages,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum CliTheme {
    /// Choose a built-in theme from terminal background hints when available.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// Use the built-in dark-background theme.
    #[serde(rename = "dark")]
    Dark,
    /// Use the built-in light-background theme.
    #[serde(rename = "light")]
    Light,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ShowMessages {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "self-summary")]
    SelfSummary,
    #[serde(rename = "self-full")]
    SelfFull,
    #[serde(rename = "all-summary")]
    AllSummary,
    #[serde(rename = "all-full")]
    #[default]
    AllFull,
}

impl ShowMessages {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SelfSummary => "self-summary",
            Self::SelfFull => "self-full",
            Self::AllSummary => "all-summary",
            Self::AllFull => "all-full",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "self-summary" => Some(Self::SelfSummary),
            "self-full" => Some(Self::SelfFull),
            "all-summary" => Some(Self::AllSummary),
            "all-full" => Some(Self::AllFull),
            _ => None,
        }
    }
}

impl Default for CliState {
    fn default() -> Self {
        Self {
            show_diff: false,
            show_thinking: true,
            show_turn_stats: false,
            redraw_counter: false,
            show_tools: ShowTools::Full,
            show_messages: ShowMessages::AllFull,
        }
    }
}

impl CliState {
    /// Load the persisted CLI state. Missing / malformed file → defaults.
    #[must_use]
    pub fn load(dirs: &TauDirs) -> Self {
        Self::load_with_default(dirs, Self::default())
    }

    /// Load the persisted CLI state, using `default` when state is missing or
    /// malformed. This lets static CLI config provide the initial values while
    /// `/set` changes still persist as runtime state.
    #[must_use]
    pub fn load_with_default(dirs: &TauDirs, default: Self) -> Self {
        let Some(dir) = dirs.state_dir.as_ref() else {
            return default;
        };
        let path = dir.join("cli.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return default;
        };
        serde_json::from_str(&text).unwrap_or(default)
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

/// Harness/agent settings loaded from `harness.yaml`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.harness.yaml` and is layered in by the loader. Use
/// [`HarnessSettings::built_in`] when you need a fresh, populated value in a
/// test or fallback.
#[derive(Clone, Debug)]
pub struct HarnessSettings {
    /// Number of days to keep inactive session state directories.
    /// Set to `0` to disable session cleanup.
    pub session_retention_days: u64,

    /// Extension table, keyed by name. Built-in entries (`provider-builtin`,
    /// `core-shell`) come pre-baked at the harness level; anything the
    /// user writes here overrides those per-field, or adds a new
    /// extension.
    ///
    /// Example `harness.yaml`:
    /// ```yaml
    /// extensions:
    ///   core-shell:
    ///     enable: false
    ///   provider-builtin:
    ///     prefix: ["ssh", "user@host"]
    ///   mything:
    ///     command: ["/usr/local/bin/my-tau-ext"]
    /// ```
    pub extensions: HashMap<String, ExtensionEntry>,

    /// Role selected on startup when no explicit runtime selection has been
    /// made. If the configured role is missing, Tau warns and falls back to
    /// the first role in `roleGroups` order.
    pub default_role: Option<String>,

    /// Harness-owned role defaults. Each role is a partial set of model
    /// settings; missing fields use provider/model fallbacks for the selected
    /// provider-published model.
    pub roles: HashMap<String, AgentRole>,

    /// Ordered role groups used by the CLI for structured role navigation.
    /// Role names remain globally unique; groups provide shared defaults for
    /// their `roles` entries and affect presentation and keyboard cycling.
    pub role_groups: Vec<RoleGroup>,

    /// Top-level prompt fragments from harness config. Loaded settings also
    /// fold these into every role's prompt fragments; this field preserves the
    /// global source list for inspection and future config tooling.
    pub prompt_fragments: Vec<RolePromptFragment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessSettingsWire {
    session_retention_days: u64,
    extensions: HashMap<String, ExtensionEntry>,
    #[serde(default, rename = "defaultRole")]
    default_role: Option<String>,
    #[serde(default, rename = "roleGroups")]
    role_groups: RawRoleGroups,
    #[serde(default, rename = "promptFragments")]
    prompt_fragments: Vec<RolePromptFragment>,
}

impl<'de> Deserialize<'de> for HarnessSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HarnessSettingsWire::deserialize(deserializer)?;
        let mut settings = Self {
            session_retention_days: wire.session_retention_days,
            extensions: wire.extensions,
            default_role: wire.default_role,
            roles: HashMap::new(),
            role_groups: Vec::new(),
            prompt_fragments: wire.prompt_fragments,
        };
        settings
            .apply_role_group_overrides(wire.role_groups)
            .map_err(D::Error::custom)?;
        settings.remove_disabled_roles();
        Ok(settings)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessRoleOverrides {
    #[serde(default, rename = "session_retention_days")]
    _session_retention_days: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "extensions")]
    _extensions: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "defaultRole")]
    _default_role: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "roleGroups")]
    role_groups: RawRoleGroups,
    #[serde(default, rename = "promptFragments")]
    prompt_fragments: Vec<RolePromptFragment>,
}

/// One ordered group in the role navigation palette.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleGroup {
    /// Stable group name from `roleGroups.<name>`.
    pub name: String,
    /// Globally unique role names in this group, in configured order.
    pub roles: Vec<String>,
}

type RawRoleGroups = IndexMap<String, RawRoleGroup>;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawRoleGroup {
    // `enabled` was a mistaken old spelling. Keep it as a little bandaid for
    // reading old config during migration.
    #[serde(alias = "enabled")]
    enable: Option<bool>,
    description: Option<String>,
    model: Option<ModelId>,
    effort: Option<tau_proto::Effort>,
    verbosity: Option<tau_proto::Verbosity>,
    #[serde(rename = "thinkingSummary")]
    thinking_summary: Option<tau_proto::ThinkingSummary>,
    #[serde(rename = "serviceTier")]
    service_tier: Option<tau_proto::ServiceTier>,
    #[serde(default, deserialize_with = "deserialize_optional_percent")]
    compaction_threshold: Option<u8>,
    prompt_fragments: Vec<RolePromptFragment>,
    #[serde(rename = "promptOverride")]
    prompt_override: Option<String>,
    tools: Option<Vec<ToolName>>,
    #[serde(rename = "disableTools")]
    disable_tools: Vec<ToolName>,
    roles: IndexMap<String, AgentRole>,
}

impl RawRoleGroup {
    fn defaults(&self) -> AgentRole {
        AgentRole {
            enable: self.enable,
            description: self.description.clone(),
            model: self.model.clone(),
            effort: self.effort,
            verbosity: self.verbosity,
            thinking_summary: self.thinking_summary,
            service_tier: self.service_tier,
            compaction_threshold: self.compaction_threshold,
            prompt_fragments: self.prompt_fragments.clone(),
            prompt_override: self.prompt_override.clone(),
            tools: self.tools.clone(),
            disable_tools: self.disable_tools.clone(),
        }
    }
}

/// One command-line role availability override, applied after all config files.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RoleCliOverride {
    /// Enable a named role in the effective role set.
    Enable(String),
    /// Disable a named role in the effective role set.
    Disable(String),
    /// Disable all roles before later command-line role overrides are applied.
    DisableAll,
}

impl HarnessSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.harness.yaml`.
    pub fn built_in() -> Self {
        let mut s: Self = parse_built_in_yaml("built-in.harness.yaml", BUILT_IN_HARNESS_YAML);
        s.remove_disabled_roles();
        s.apply_global_prompt_fragments_to_roles();
        s
    }

    fn apply_role_group_overrides(&mut self, groups: RawRoleGroups) -> Result<(), SettingsError> {
        for (group_name, group) in groups {
            let group_defaults = group.defaults();
            if group.roles.is_empty() {
                if let Some(existing_group) = self
                    .role_groups
                    .iter()
                    .find(|existing_group| existing_group.name == group_name)
                {
                    for role_name in existing_group.roles.clone() {
                        if let Some(role) = self.roles.get_mut(&role_name) {
                            role.apply_overrides_from(&group_defaults);
                        }
                    }
                } else {
                    self.role_groups.push(RoleGroup {
                        name: group_name,
                        roles: Vec::new(),
                    });
                }
                continue;
            }
            for (role_name, role_overrides) in group.roles {
                let mut override_role = group_defaults.clone();
                override_role.apply_overrides_from(&role_overrides);
                self.ensure_role_group_member(&group_name, &role_name)?;
                self.roles
                    .entry(role_name)
                    .and_modify(|role| role.apply_overrides_from(&override_role))
                    .or_insert(override_role);
            }
        }
        Ok(())
    }

    fn apply_role_cli_overrides(&mut self, overrides: &[RoleCliOverride]) {
        for override_ in overrides {
            match override_ {
                RoleCliOverride::Enable(role_name) => {
                    if let Some(role) = self.roles.get_mut(role_name) {
                        role.enable = Some(true);
                    }
                }
                RoleCliOverride::Disable(role_name) => {
                    if let Some(role) = self.roles.get_mut(role_name) {
                        role.enable = Some(false);
                    }
                }
                RoleCliOverride::DisableAll => {
                    for role in self.roles.values_mut() {
                        role.enable = Some(false);
                    }
                }
            }
        }
    }

    fn remove_disabled_roles(&mut self) {
        self.roles
            .retain(|_role_name, role| role.enable.unwrap_or(true));
        for group in &mut self.role_groups {
            group
                .roles
                .retain(|role_name| self.roles.contains_key(role_name));
        }
        self.role_groups.retain(|group| !group.roles.is_empty());
    }

    fn ensure_role_group_member(
        &mut self,
        group_name: &str,
        role_name: &str,
    ) -> Result<(), SettingsError> {
        for group in &mut self.role_groups {
            if group.roles.iter().any(|existing| existing == role_name) {
                if group.name == group_name {
                    return Ok(());
                }
                return Err(SettingsError::DuplicateGroupedRole {
                    role: role_name.to_owned(),
                    first_group: group.name.clone(),
                    second_group: group_name.to_owned(),
                });
            }
        }

        if let Some(group) = self
            .role_groups
            .iter_mut()
            .find(|group| group.name == group_name)
        {
            group.roles.push(role_name.to_owned());
        } else {
            self.role_groups.push(RoleGroup {
                name: group_name.to_owned(),
                roles: vec![role_name.to_owned()],
            });
        }
        Ok(())
    }

    fn apply_prompt_fragment_overrides(&mut self, fragments: Vec<RolePromptFragment>) {
        for prompt_fragment in fragments {
            if !self.prompt_fragments.contains(&prompt_fragment) {
                self.prompt_fragments.push(prompt_fragment);
            }
        }
    }

    fn apply_global_prompt_fragments_to_roles(&mut self) {
        for role in self.roles.values_mut() {
            for prompt_fragment in &self.prompt_fragments {
                if !role.prompt_fragments.contains(prompt_fragment) {
                    role.prompt_fragments.push(prompt_fragment.clone());
                }
            }
        }
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
/// fields they care about for built-in extensions; the harness merges
/// these with built-in defaults at startup. `None` on any field means
/// "the user did not say anything" — distinct from an empty value the
/// user set on purpose.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionEntry {
    /// argv prefix prepended before `command`. Useful for wrappers
    /// that don't change the inner command, e.g.
    /// `["ssh", "user@host"]` to run remotely or
    /// `["bwrap", "--ro-bind", "/", "/", "--"]` to sandbox.
    pub prefix: Option<Vec<String>>,

    /// argv of the extension itself. `command[0]` is the executable;
    /// the rest are arguments. For built-in extensions this defaults
    /// to `[<current-exe>]`; for new entries this must be set
    /// explicitly. Tau-piggybacking entries can omit `command` and
    /// use `suffix` to pick the subcommand on the running tau binary.
    pub command: Option<Vec<String>>,

    /// argv suffix appended after `command`. Symmetric to `prefix`.
    /// Built-in extensions use this to spell their subcommand (e.g.
    /// `["ext", "ext-provider-builtin"]`) so the `command` slot stays
    /// as the tau binary path.
    pub suffix: Option<Vec<String>>,

    /// Whether to run this extension. Defaults to the built-in's
    /// `enable` (or `true` for user-added entries). Set to `false`
    /// to keep the entry in config but skip spawning.
    pub enable: Option<bool>,

    /// Role tag. Built-in providers use `role: "provider"`; entries
    /// without that role are treated as tool extensions.
    pub role: Option<String>,

    /// Free-form configuration object handed to the extension at
    /// startup via `LifecycleConfigure`. The harness does not
    /// interpret it — the extension defines and validates its own
    /// schema. Absent on the wire means "merge nothing in", so the
    /// built-in's default config object is used unchanged.
    pub config: Option<serde_json::Value>,

    /// Secret names this extension is allowed to receive, keyed by secret name.
    pub secrets: Option<BTreeMap<String, ExtensionSecretEntry>>,
}

/// Per-secret declaration for one extension.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionSecretEntry {
    /// Whether startup may continue when this secret is unavailable. Required
    /// by default.
    pub optional: bool,
}

// ---------------------------------------------------------------------------
// Harness roles
// ---------------------------------------------------------------------------

/// Partial harness role settings loaded from `harness.yaml` and persisted
/// to state. `None` means "use the selected model's fallback" for every field.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentRole {
    /// Whether this role is part of the effective runtime role set. Defaults to
    /// enabled; set to `false` in a higher-precedence config layer to hide a
    /// built-in or lower-layer role without deleting the rest of its settings.
    ///
    /// `enabled` was a mistaken old spelling. Keep it as a little bandaid for
    /// reading old config during migration.
    #[serde(alias = "enabled", skip_serializing_if = "Option::is_none")]
    pub enable: Option<bool>,
    /// Short free-form summary shown in role-selection completion menus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Model id preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Reasoning effort preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<tau_proto::Effort>,
    /// Output verbosity preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<tau_proto::Verbosity>,
    /// Thinking-summary mode preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingSummary")]
    pub thinking_summary: Option<tau_proto::ThinkingSummary>,
    /// Provider service tier preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none", rename = "serviceTier")]
    pub service_tier: Option<tau_proto::ServiceTier>,
    /// Context-window percentage at which automatic compaction should start.
    /// Missing values use Tau's default threshold.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "compactionThreshold",
        deserialize_with = "deserialize_optional_percent"
    )]
    pub compaction_threshold: Option<u8>,
    /// Prompt fragments contributed by this role. Fragments are rendered as
    /// Handlebars templates and ordered together with tool/extension fragments.
    #[serde(skip_serializing_if = "Vec::is_empty", rename = "promptFragments")]
    pub prompt_fragments: Vec<RolePromptFragment>,
    /// Optional system prompt template name for this role. "built-in" selects
    /// Tau's embedded default template. Other names resolve to
    /// `<config_dir>/prompts/<name>.hbs`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "promptOverride")]
    pub prompt_override: Option<String>,
    /// Explicit internal tool names enabled for this role. When unset, tools
    /// use their own default enablement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolName>>,
    /// Internal tool names disabled for this role even if selected or enabled
    /// by default.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "disableTools"
    )]
    pub disable_tools: Vec<ToolName>,
}

fn deserialize_optional_percent<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u8>::deserialize(deserializer)?;
    if let Some(percent) = value
        && percent > 100
    {
        return Err(D::Error::invalid_value(
            Unexpected::Unsigned(u64::from(percent)),
            &"a percentage from 0 to 100",
        ));
    }
    Ok(value)
}

impl AgentRole {
    fn apply_overrides_from(&mut self, override_role: &Self) {
        if let Some(enable) = override_role.enable {
            self.enable = Some(enable);
        }
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
        if let Some(compaction_threshold) = override_role.compaction_threshold {
            self.compaction_threshold = Some(compaction_threshold);
        }
        for prompt_fragment in &override_role.prompt_fragments {
            if !self.prompt_fragments.contains(prompt_fragment) {
                self.prompt_fragments.push(prompt_fragment.clone());
            }
        }
        if let Some(prompt_override) = &override_role.prompt_override {
            self.prompt_override = Some(prompt_override.clone());
        }
        if let Some(tools) = &override_role.tools {
            self.tools = Some(tools.clone());
        }
        if !override_role.disable_tools.is_empty() {
            self.disable_tools = override_role.disable_tools.clone();
        }
    }
}

/// One prompt fragment configured on a harness role.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RolePromptFragment {
    /// Stable fragment name, preferably namespaced by role or purpose.
    pub name: String,
    /// Priority controlling placement among all prompt fragments. Lower values
    /// render earlier. Values below 100 are intended for role/persona
    /// instructions that should precede generated context; high values are for
    /// epilogue-style context such as the current working directory.
    pub priority: PromptPriority,
    /// Handlebars template text rendered into the system prompt.
    pub text: PromptContent,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Errors from settings loading.
#[derive(Debug)]
pub enum SettingsError {
    Config(config::ConfigError),
    DuplicateGroupedRole {
        role: String,
        first_group: String,
        second_group: String,
    },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(source) => write!(f, "settings error: {source}"),
            Self::DuplicateGroupedRole {
                role,
                first_group,
                second_group,
            } => write!(
                f,
                "role `{role}` appears in multiple roleGroups (`{first_group}` and `{second_group}`)"
            ),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::DuplicateGroupedRole { .. } => None,
        }
    }
}

impl From<config::ConfigError> for SettingsError {
    fn from(source: config::ConfigError) -> Self {
        Self::Config(source)
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

/// Returns the persistent state directory reserved for one extension.
///
/// The harness passes this path to the extension in
/// [`tau_proto::Configure::state_dir`]. Extension names come from the resolved
/// harness configuration, including user-authored `harness.yaml` keys, so only
/// conservative single-component names are accepted before joining under
/// `state/ext/`.
pub fn extension_state_dir_of(
    state_dir: &Path,
    extension_name: &str,
) -> Result<PathBuf, InvalidExtensionName> {
    validate_extension_name(extension_name)?;
    Ok(state_dir.join("ext").join(extension_name))
}

/// Validates that an extension name is safe to use as a single path component
/// in harness-owned per-extension paths.
pub fn validate_extension_name(extension_name: &str) -> Result<(), InvalidExtensionName> {
    if extension_name.is_empty() {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name must not be empty",
        });
    }
    if extension_name == "." || extension_name == ".." {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name must be a normal path component",
        });
    }
    if !extension_name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name may contain only ASCII letters, digits, '.', '_' and '-'",
        });
    }
    Ok(())
}

/// Error returned when a configured extension name is unsafe to use as a state
/// directory path component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidExtensionName {
    name: String,
    reason: &'static str,
}

impl fmt::Display for InvalidExtensionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid extension name `{}` for harness path component: {}",
            self.name, self.reason
        )
    }
}

impl std::error::Error for InvalidExtensionName {}

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
    /// Where to look for `cli.yaml`, `harness.yaml`, etc.
    pub config_dir: Option<PathBuf>,
    /// Where to read/write runtime state like persisted role settings.
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

/// Loads CLI settings from `cli.yaml` with `cli.d/*.yaml` overrides.
pub fn load_cli_settings() -> Result<CliSettings, SettingsError> {
    load_cli_settings_in(&TauDirs::default())
}

/// Like [`load_cli_settings`] but reads from an explicit directory layout.
///
/// The embedded `built-in.cli.yaml` is layered underneath the user's
/// own `cli.yaml` (and any `cli.d/*.yaml` drop-ins), so the user
/// can write a partial file and unmentioned fields fall back to the
/// shipped defaults. The `bind` map is merged per-key on top so a
/// user customizing one chord doesn't lose the others.
pub fn load_cli_settings_in(dirs: &TauDirs) -> Result<CliSettings, SettingsError> {
    let mut settings: CliSettings =
        load_yaml_layered_with_builtin(BUILT_IN_CLI_YAML, dirs.config_dir.as_deref(), "cli")?;
    let mut bindings = default_cli_bindings();
    bindings.extend(settings.bind);
    settings.bind = bindings;
    Ok(settings)
}

/// Loads harness settings from `harness.yaml` with `harness.d/*.yaml`
/// overrides.
pub fn load_harness_settings() -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_in(&TauDirs::default())
}

/// Like [`load_harness_settings`] but reads from an explicit directory layout.
pub fn load_harness_settings_in(dirs: &TauDirs) -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_with_role_overrides_in(dirs, &[])
}

/// Like [`load_harness_settings_in`], then applies role CLI overrides in order.
pub fn load_harness_settings_with_role_overrides_in(
    dirs: &TauDirs,
    role_overrides: &[RoleCliOverride],
) -> Result<HarnessSettings, SettingsError> {
    let mut settings: HarnessSettings = load_yaml_layered_with_builtin(
        BUILT_IN_HARNESS_YAML,
        dirs.config_dir.as_deref(),
        "harness",
    )?;

    // Generic YAML layering replaces arrays, but prompt fragments are additive
    // metadata. Recompute roles and top-level prompt fragments through the
    // domain merge path; all other harness fields keep normal config-layer
    // semantics.
    let mut role_settings = HarnessSettings::built_in();
    for overrides in
        load_yaml_layer_files::<HarnessRoleOverrides>(dirs.config_dir.as_deref(), "harness")?
    {
        role_settings.apply_prompt_fragment_overrides(overrides.prompt_fragments);
        role_settings.apply_role_group_overrides(overrides.role_groups)?;
    }
    role_settings.apply_role_cli_overrides(role_overrides);
    role_settings.remove_disabled_roles();
    role_settings.apply_global_prompt_fragments_to_roles();
    settings.prompt_fragments = role_settings.prompt_fragments;
    settings.roles = role_settings.roles;
    settings.role_groups = role_settings.role_groups;
    Ok(settings)
}

/// Stacks an embedded built-in YAML string underneath the user's files.
/// `T` therefore doesn't need a `Default` impl — the built-in layer always
/// supplies every required field.
fn load_yaml_layered_with_builtin<T: for<'de> Deserialize<'de>>(
    built_in_text: &'static str,
    dir: Option<&Path>,
    name: &str,
) -> Result<T, SettingsError> {
    let builder = config::Config::builder()
        .add_source(config::File::from_str(built_in_text, config::FileFormat::Yaml).required(true));
    let builder = add_yaml_file_sources(builder, dir, name);
    builder
        .build()?
        .try_deserialize()
        .map_err(SettingsError::from)
}

fn load_yaml_layer_files<T: for<'de> Deserialize<'de>>(
    dir: Option<&Path>,
    name: &str,
) -> Result<Vec<T>, SettingsError> {
    yaml_layer_paths(dir, name)
        .into_iter()
        .map(|path| {
            config::Config::builder()
                .add_source(
                    config::File::from(path)
                        .format(config::FileFormat::Yaml)
                        .required(true),
                )
                .build()?
                .try_deserialize()
                .map_err(SettingsError::from)
        })
        .collect()
}

fn add_yaml_file_sources(
    mut builder: config::ConfigBuilder<config::builder::DefaultState>,
    dir: Option<&Path>,
    name: &str,
) -> config::ConfigBuilder<config::builder::DefaultState> {
    for path in yaml_layer_paths(dir, name) {
        builder = builder.add_source(
            config::File::from(path)
                .format(config::FileFormat::Yaml)
                .required(true),
        );
    }
    builder
}

fn yaml_layer_paths(dir: Option<&Path>, name: &str) -> Vec<PathBuf> {
    let Some(dir) = dir else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    let base_path = dir.join(format!("{name}.yaml"));
    if base_path.exists() {
        paths.push(base_path);
    }

    let drop_dir = dir.join(format!("{name}.d"));
    if drop_dir.is_dir() {
        let mut drop_in_paths: Vec<PathBuf> = std::fs::read_dir(&drop_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|ext| ext == "yaml" || ext == "yml")
            })
            .collect();
        drop_in_paths.sort();
        paths.extend(drop_in_paths);
    }
    paths
}

#[cfg(test)]
mod tests;
