//! Loading and resolving harness/extension configuration on startup.
//!
//! Owns the resolved-configuration types ([`Config`], [`CoreConfig`],
//! [`CoreMode`], [`ExtensionConfig`]), the built-in extension list, and
//! the resolver that merges the user's
//! [`tau_config::settings::HarnessSettings`] on top of the built-ins. The wire
//! schema for `harness.yaml` lives in `tau-config`; this module turns that
//! schema into something the harness can spawn.

use std::collections::BTreeMap;
use std::fmt;

use tau_config::settings::{
    ExtensionCliOverride, ExtensionEntry, ExtensionSecretEntry, HarnessSettings, RoleCliOverride,
};

/// The resolved harness configuration handed to the daemon.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub core: CoreConfig,
    pub extensions: BTreeMap<String, ExtensionConfig>,
}

/// Resolved core configuration values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreConfig {
    pub mode: CoreMode,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            mode: CoreMode::Embedded,
        }
    }
}

/// Minimal runtime mode selection for the harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreMode {
    Embedded,
    Daemon,
}

/// One configured extension process, after merging built-in defaults
/// and user overrides. Ready to spawn.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub role: Option<String>,
    /// Config object handed to the extension via
    /// `LifecycleConfigure`. Defaults to an empty object so
    /// extensions always see a value.
    pub config: serde_json::Value,
    /// Secret declarations authorized for this extension.
    pub secrets: BTreeMap<String, ExtensionSecretEntry>,
}

/// Built-in extension shipped with `tau`. Used by
/// [`resolve_extensions`] to seed the table before applying user
/// overrides. argv = `prefix ++ command ++ suffix`.
pub struct BuiltinExtension {
    pub name: String,
    pub prefix: Vec<String>,
    pub command: Vec<String>,
    pub suffix: Vec<String>,
    pub role: Option<String>,
    pub enable: bool,
    /// Built-in default config for this extension, merged below any
    /// user-provided `config: { … }` object in `harness.yaml`.
    pub config: serde_json::Value,
    /// Built-in secret declarations for this extension.
    pub secrets: BTreeMap<String, ExtensionSecretEntry>,
}

/// Error returned by [`resolve_extensions`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveExtensionsError {
    /// A user-added extension entry has no `command` (and therefore
    /// no executable to spawn).
    EmptyCommand(String),
}

impl fmt::Display for ResolveExtensionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand(name) => write!(
                f,
                "extension {name:?} has no `command` set; user-added entries must specify the executable",
            ),
        }
    }
}

impl std::error::Error for ResolveExtensionsError {}

#[derive(Debug)]
struct ResolvedExtension {
    prefix: Vec<String>,
    command: Vec<String>,
    suffix: Vec<String>,
    enable: bool,
    role: Option<String>,
    config: serde_json::Value,
    secrets: BTreeMap<String, ExtensionSecretEntry>,
}

/// Merge user-provided `extensions` entries on top of the supplied
/// built-in extensions and produce a flat list of [`ExtensionConfig`]s
/// ready for the harness to spawn.
///
/// Per-key merging:
/// - Field-level overlay for built-in keys: only fields the user explicitly set
///   (`Some(_)` after deserialization) replace the built-in's value. Absent
///   fields keep the built-in's defaults.
/// - User keys not in the built-in list are added as-is. They must specify a
///   non-empty `command`. Their `enable` defaults to `true`.
/// - Entries with a resolved `enable: false` are dropped.
///
/// Returns `Err` for enabled entries that end up with an empty `command` after
/// the merge — only possible for user-added unknown keys. Disabled user-added
/// entries are inert and are dropped before command validation.
pub fn resolve_extensions(
    settings: &HarnessSettings,
    builtins: Vec<BuiltinExtension>,
) -> Result<Vec<ExtensionConfig>, ResolveExtensionsError> {
    resolve_extensions_with_cli_overrides(settings, builtins, &[])
}

pub fn resolve_extensions_with_cli_overrides(
    settings: &HarnessSettings,
    builtins: Vec<BuiltinExtension>,
    cli_overrides: &[ExtensionCliOverride],
) -> Result<Vec<ExtensionConfig>, ResolveExtensionsError> {
    use std::collections::HashMap;

    // Pass 1: seed an indexed map with built-ins, in order.
    let mut order: Vec<String> = builtins.iter().map(|b| b.name.clone()).collect();
    let mut entries: HashMap<String, ResolvedExtension> = builtins
        .into_iter()
        .map(|b| {
            (
                b.name,
                ResolvedExtension {
                    prefix: b.prefix,
                    command: b.command,
                    suffix: b.suffix,
                    enable: b.enable,
                    role: b.role,
                    config: b.config,
                    secrets: b.secrets,
                },
            )
        })
        .collect();

    // Pass 2: overlay user entries. Sort user keys deterministically.
    let mut user_keys: Vec<&String> = settings.extensions.keys().collect();
    user_keys.sort();
    for name in user_keys {
        let user: &ExtensionEntry = &settings.extensions[name];
        match entries.get_mut(name) {
            Some(existing) => {
                if let Some(prefix) = user.prefix.as_ref() {
                    existing.prefix = prefix.clone();
                }
                if let Some(command) = user.command.as_ref() {
                    existing.command = command.clone();
                    // Setting `command` replaces the built-in's full argv tail.
                    // `suffix` is cleared so users overriding only `command`
                    // don't accidentally inherit the built-in's subcommand
                    // tokens (e.g. `["ext", "ext-provider-builtin"]`). Users
                    // who want to keep them must set `suffix` explicitly below.
                    existing.suffix = Vec::new();
                }
                if let Some(suffix) = user.suffix.as_ref() {
                    existing.suffix = suffix.clone();
                }
                if let Some(enable) = user.enable {
                    existing.enable = enable;
                }
                if let Some(role) = user.role.as_ref() {
                    existing.role = Some(role.clone());
                }
                if let Some(over) = user.config.clone() {
                    existing.config = merge_json(existing.config.take(), over);
                }
                if let Some(secrets) = user.secrets.as_ref() {
                    existing.secrets.extend(secrets.clone());
                }
            }
            None => {
                let command = user.command.clone().unwrap_or_default();
                order.push(name.clone());
                entries.insert(
                    name.clone(),
                    ResolvedExtension {
                        prefix: user.prefix.clone().unwrap_or_default(),
                        command,
                        suffix: user.suffix.clone().unwrap_or_default(),
                        enable: user.enable.unwrap_or(true),
                        role: user.role.clone(),
                        config: user
                            .config
                            .clone()
                            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
                        secrets: user.secrets.clone().unwrap_or_default(),
                    },
                );
            }
        }
    }

    // Pass 3: apply command-line availability overrides in argument order.
    // Unknown named extensions are ignored, matching role CLI override behavior.
    for override_ in cli_overrides {
        match override_ {
            ExtensionCliOverride::Enable(extension_name) => {
                if let Some(entry) = entries.get_mut(extension_name) {
                    entry.enable = true;
                }
            }
            ExtensionCliOverride::Disable(extension_name) => {
                if let Some(entry) = entries.get_mut(extension_name) {
                    entry.enable = false;
                }
            }
            ExtensionCliOverride::EnableAll => {
                for entry in entries.values_mut() {
                    entry.enable = true;
                }
            }
            ExtensionCliOverride::DisableAll => {
                for entry in entries.values_mut() {
                    entry.enable = false;
                }
            }
        }
    }

    // Pass 4: produce ExtensionConfigs in declared order, dropping
    // disabled entries. argv = prefix ++ command ++ suffix; argv[0]
    // is the executable, rest are args.
    let mut out = Vec::new();
    for name in order {
        let entry = entries.remove(&name).expect("seeded above");
        if !entry.enable {
            continue;
        }
        let mut argv = entry.prefix;
        argv.extend(entry.command);
        argv.extend(entry.suffix);
        let (program, args) = match argv.split_first() {
            Some((first, rest)) => (first.clone(), rest.to_vec()),
            None => return Err(ResolveExtensionsError::EmptyCommand(name)),
        };
        out.push(ExtensionConfig {
            name,
            command: program,
            args,
            role: entry.role,
            config: entry.config,
            secrets: entry.secrets,
        });
    }
    Ok(out)
}

/// Merge `over` on top of `base` for extension config objects.
///
/// When both are JSON objects, keys are merged shallowly:
/// `over`'s keys win, `base`'s keys are kept where `over` doesn't
/// mention them. For any other shape (one side isn't an object),
/// `over` replaces `base` outright if it isn't `Null`. This is the
/// minimum needed to let a user override one field of a builtin's
/// config without restating the rest.
fn merge_json(base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
    match (base, over) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                b.insert(k, v);
            }
            serde_json::Value::Object(b)
        }
        (base, serde_json::Value::Null) => base,
        (_, over) => over,
    }
}

/// Load `harness.yaml`, falling back to defaults on parse error and
/// writing a warning to stderr. Returns the parse error too so the
/// harness can surface it in the UI without re-parsing the same file
/// from scratch.
///
/// Without the warning a malformed file silently disables every
/// user-configured extension and the only symptom is "my extension
/// isn't running" with no clue why.
pub const ROLE_CLI_OVERRIDES_ENV: &str = "TAU_ROLE_CLI_OVERRIDES";
pub const EXTENSION_CLI_OVERRIDES_ENV: &str = "TAU_EXTENSION_CLI_OVERRIDES";

pub(crate) fn load_harness_settings_or_warn(
    dirs: &tau_config::settings::TauDirs,
) -> (HarnessSettings, Option<tau_config::settings::SettingsError>) {
    let role_overrides = role_cli_overrides_from_env();
    match tau_config::settings::load_harness_settings_with_role_overrides_in(dirs, &role_overrides)
    {
        Ok(settings) => (settings, None),
        Err(error) => {
            eprintln!("tau: harness.yaml failed to parse — ignored.\n{error}");
            (HarnessSettings::built_in(), Some(error))
        }
    }
}

fn role_cli_overrides_from_env() -> Vec<RoleCliOverride> {
    std::env::var(ROLE_CLI_OVERRIDES_ENV)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn extension_cli_overrides_from_env() -> Vec<ExtensionCliOverride> {
    std::env::var(EXTENSION_CLI_OVERRIDES_ENV)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

/// The set of extensions the harness ships with by default.
///
/// Each entry's `command` is `[<current-exe>]` and `suffix` is
/// `["ext", <name>]`, so a fresh `tau` install with no
/// `harness.yaml` runs the in-binary provider and tool extensions out
/// of the box. Users can override individual fields
/// (or set `enable: false`) per entry in `harness.yaml` under
/// `extensions: { name: { … } }`.
///
/// The list itself lives in `config/built-in.extensions.json5` and is
/// embedded into the binary via `include_str!`; `built_in_extension_defs`
/// performs the parse step.
#[must_use]
pub fn builtin_extensions() -> Vec<BuiltinExtension> {
    let tau_binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "tau".to_owned());

    built_in_extension_defs()
        .iter()
        .map(|def| BuiltinExtension {
            name: def.name.clone(),
            prefix: def.prefix.clone().unwrap_or_default(),
            command: def
                .command
                .clone()
                .unwrap_or_else(|| vec![tau_binary.clone()]),
            suffix: def.suffix.clone().unwrap_or_default(),
            role: def.role.clone(),
            enable: def.enable,
            config: def.config.clone(),
            secrets: def.secrets.clone().unwrap_or_default(),
        })
        .collect()
}

const BUILT_IN_EXTENSIONS_JSON5: &str = include_str!("../config/built-in.extensions.json5");

/// Wire schema for one entry in `built-in.extensions.json5`. `command`
/// is optional — when omitted, [`builtin_extensions`] substitutes
/// `[<current-exe>]` so the built-in runs the tau binary itself.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuiltInExtensionDef {
    pub name: String,
    #[serde(default)]
    pub prefix: Option<Vec<String>>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub suffix: Option<Vec<String>>,
    #[serde(default)]
    pub role: Option<String>,
    pub enable: bool,
    pub config: serde_json::Value,
    #[serde(default)]
    pub secrets: Option<BTreeMap<String, ExtensionSecretEntry>>,
}

pub(crate) fn built_in_extension_defs() -> &'static [BuiltInExtensionDef] {
    static B: std::sync::LazyLock<Vec<BuiltInExtensionDef>> = std::sync::LazyLock::new(|| {
        json5::from_str(BUILT_IN_EXTENSIONS_JSON5).unwrap_or_else(|err| {
            panic!(
                "tau ships with malformed built-in.extensions.json5: {err}\n\
                 this is a bug; please report it"
            )
        })
    });
    &B
}

#[must_use]
pub fn default_config() -> Config {
    // `resolve_extensions` is fallible only for user-added entries with an
    // empty `command`. Here we pass an empty `HarnessSettings` and the
    // hard-coded `builtin_extensions()` list (all with non-empty `command`),
    // so the failure path is unreachable.
    let extensions = match resolve_extensions(&HarnessSettings::built_in(), builtin_extensions()) {
        Ok(extensions) => extensions,
        Err(err) => unreachable!("built-in extensions resolve cleanly: {err}"),
    };

    Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions: extensions
            .into_iter()
            .map(|extension| (extension.name.clone(), extension))
            .collect(),
    }
}

pub(crate) fn resolve_config(
    _explicit_path: Option<&std::path::Path>,
) -> Result<Config, Box<dyn std::error::Error>> {
    // Extensions live in `harness.yaml` under `extensions: { ... }`.
    // We start from the built-in provider + tools defaults and apply the
    // user's overrides on top; a malformed harness.yaml falls back
    // to defaults rather than failing the whole startup, but we warn
    // on stderr so the user can see why their config is being
    // ignored.
    let (settings, _) = load_harness_settings_or_warn(&tau_config::settings::TauDirs::default());
    let extension_overrides = extension_cli_overrides_from_env();
    let extensions = resolve_extensions_with_cli_overrides(
        &settings,
        builtin_extensions(),
        &extension_overrides,
    )?;
    Ok(Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions: extensions
            .into_iter()
            .map(|extension| (extension.name.clone(), extension))
            .collect(),
    })
}

#[cfg(test)]
mod tests;
