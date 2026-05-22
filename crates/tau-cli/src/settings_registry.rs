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
    /// (re-render diff blocks vs. status bar vs. turn-stats blocks)
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

const SHOW_MESSAGES_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "none",
        description: "hide all messages",
    },
    SettingValue {
        value: "self-summary",
        description: "summarize messages from or to the user",
    },
    SettingValue {
        value: "self-full",
        description: "show messages from or to the user",
    },
    SettingValue {
        value: "all-summary",
        description: "show user messages, summarize agent-agent messages",
    },
    SettingValue {
        value: "all-full",
        description: "show all messages",
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
        name: "show-turn-stats",
        description: "Turn stats below agent responses",
        values: BOOL_VALUES,
        get: |s| bool_str(s.show_turn_stats),
    },
    SettingDef {
        name: "redraw-counter",
        description: "Temporary full-redraw counter in the status bar",
        values: BOOL_VALUES,
        get: |s| bool_str(s.redraw_counter),
    },
    SettingDef {
        name: "show-tools",
        description: "Tool block visibility",
        values: SHOW_TOOLS_VALUES,
        get: |s| s.show_tools.as_str(),
    },
    SettingDef {
        name: "show-messages",
        description: "Agent message visibility",
        values: SHOW_MESSAGES_VALUES,
        get: |s| s.show_messages.as_str(),
    },
];

pub fn find(name: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    /// `/set show-messages` is registry-driven, so the registry must expose all
    /// documented modes for parsing and completion.
    #[test]
    fn show_messages_values_are_registered() {
        let setting = super::find("show-messages").expect("show-messages setting");
        let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

        assert_eq!(
            values,
            vec![
                "none",
                "self-summary",
                "self-full",
                "all-summary",
                "all-full"
            ]
        );
    }
}
