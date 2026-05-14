//! User-tweakable UI settings exposed through `/set <name> <value>`.
//!
//! Each [`SettingDef`] knows how to read/write a field on [`CliState`]
//! and what its allowed values are. The registry drives both the
//! `/set` parser and completion (setting names with current values,
//! then values with descriptions).
//!
//! Most settings are booleans rendered as `true`/`false`; the shape is
//! value-list based so settings can also take three or more named values
//! without further plumbing.

use tau_config::settings::CliState;

/// One allowed value for a setting, with a short description shown in
/// the completion menu.
pub struct SettingValue {
    pub value: &'static str,
    pub description: &'static str,
}

/// Definition of a `/set`-controllable UI setting.
pub struct SettingDef {
    pub name: &'static str,
    pub description: &'static str,
    pub values: &'static [SettingValue],
    /// Read the setting's current value from `CliState`, returning the
    /// matching `values[i].value` string. Used by the completion menu
    /// to show the live value alongside each setting name. Writes go
    /// through the renderer's per-setting repaint dispatch instead of
    /// a generic setter — every setting needs a distinct invalidation
    /// (re-render diff blocks vs. status bar vs. token-stats blocks)
    /// so a `fn(&mut CliState, &str)` here wouldn't actually buy us
    /// anything.
    pub get: fn(&CliState) -> &'static str,
}

const BOOL_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "true",
        description: "enabled",
    },
    SettingValue {
        value: "false",
        description: "disabled",
    },
];

const SHOW_TOOLS_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "off",
        description: "hide tool blocks",
    },
    SettingValue {
        value: "summarize-turn",
        description: "show one summary per assistant tool turn",
    },
    SettingValue {
        value: "summarize-prompt",
        description: "show one summary per user prompt",
    },
    SettingValue {
        value: "compact",
        description: "show tool headers without payloads",
    },
    SettingValue {
        value: "full",
        description: "show every tool block",
    },
];

fn bool_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        name: "show-diff",
        description: "Expanded vs compact display of file edit diffs",
        values: BOOL_VALUES,
        get: |s| bool_str(s.show_diff),
    },
    SettingDef {
        name: "show-thinking",
        description: "Visibility of the agent's reasoning summary blocks",
        values: BOOL_VALUES,
        get: |s| bool_str(s.show_thinking),
    },
    SettingDef {
        name: "show-token-stats",
        description: "Token usage stats below agent responses",
        values: BOOL_VALUES,
        get: |s| bool_str(s.show_token_stats),
    },
    SettingDef {
        name: "show-tools",
        description: "Tool block visibility",
        values: SHOW_TOOLS_VALUES,
        get: |s| s.show_tools.as_str(),
    },
];

pub fn find(name: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|s| s.name == name)
}
