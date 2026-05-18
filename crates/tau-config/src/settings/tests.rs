use tempfile::TempDir;

use super::*;

fn dirs_with_config(dir: &std::path::Path) -> TauDirs {
    TauDirs {
        config_dir: Some(dir.to_path_buf()),
        state_dir: None,
    }
}

fn cbor_int_field(value: &CborValue, key: &str) -> Option<i128> {
    match value {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Integer(n)) if k == key => Some((*n).into()),
            _ => None,
        }),
        _ => None,
    }
}

fn cbor_bool_field(value: &CborValue, key: &str) -> Option<bool> {
    match value {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Bool(n)) if k == key => Some(*n),
            _ => None,
        }),
        _ => None,
    }
}

#[test]
fn zero_session_retention_disables_cleanup() {
    let settings = HarnessSettings {
        session_retention_days: 0,
        ..HarnessSettings::built_in()
    };

    assert_eq!(settings.session_retention(), None);
}

#[test]
fn cli_settings_user_scalar_override_wins_over_built_in() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.ncl"), r#"{ greeting = false }"#).expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
}

#[test]
fn cli_settings_user_binding_keeps_built_in_chords() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.ncl"),
        r#"{ bind = { "C-f" = { action = "shell-prompt-edit", command = "pick", trim = true } } }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    // User-overridden key reflects the user's value...
    let cf = s.bind.get("C-f").expect("C-f");
    assert_eq!(cf.action, "shell-prompt-edit");
    assert_eq!(cf.command.as_deref(), Some("pick"));
    // ...and other built-in chords survive the merge.
    assert!(s.bind.contains_key("C-r"));
    assert!(s.bind.contains_key("C-t"));
    assert!(s.bind.contains_key("C-o"));
}

#[test]
fn cli_settings_user_binding_replaces_whole_built_in_binding() {
    // Built-in bindings are defaulted per chord, not per field inside the
    // binding. A user record for a built-in chord replaces the whole binding;
    // omitted fields fall back only to CliBindingAction's serde defaults.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.ncl"),
        r#"{ bind = { "C-f" = { action = "shell-prompt-edit" } } }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    let cf = s.bind.get("C-f").expect("C-f");
    assert_eq!(cf.action, "shell-prompt-edit");
    assert_eq!(cf.command, None);
    assert!(!cf.trim);
    assert!(s.bind.contains_key("C-r"));
}

#[test]
fn cli_state_load_returns_default_when_file_missing() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    assert_eq!(CliState::load(&dirs), CliState::default());
}

#[test]
fn cli_state_round_trip_through_save_and_load() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    let original = CliState {
        show_diff: true,
        show_thinking: false,
        show_token_stats: true,
        redraw_counter: true,
        show_tools: crate::settings::ShowTools::SummarizeTurn,
    };
    original.save(&dirs);
    assert!(td.path().join("cli.json").exists());
    let reloaded = CliState::load(&dirs);
    assert_eq!(reloaded, original);
}

#[test]
fn cli_state_loads_legacy_show_tools_on_as_full() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    std::fs::write(td.path().join("cli.json"), r#"{"show_tools":"on"}"#).expect("write");

    let loaded = CliState::load(&dirs);
    assert_eq!(loaded.show_tools, crate::settings::ShowTools::Full);
}

#[test]
fn harness_settings_user_override_wins_over_built_in() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                session_retention_days = 7,
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.session_retention_days, 7);
    assert_eq!(
        s.session_retention(),
        Some(std::time::Duration::from_secs(7 * 24 * 60 * 60))
    );
}

#[test]
fn harness_settings_export_ignores_non_exported_system_prompt() {
    // The prompt template is intentionally non-exported Nickel: users may
    // override a built-in prompt with a plain record while inheriting the
    // built-in `not_exported` metadata, and the Rust HarnessSettings schema
    // must stay free of prompt fields and deserialize cleanly.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                roles = {
                    smart = {
                        systemPrompt = {
                            text = "override",
                        },
                    },
                },
            }"#,
    )
    .expect("write");

    let loaded = load_harness_settings_with_source_in(&dirs_with_config(dir)).expect("load");
    assert!(loaded.settings.roles.contains_key("smart"));
    assert!(loaded.nickel_source.contains("systemPrompt"));

    let exported: serde_json::Value =
        eval_nickel_to("composed harness raw export", &loaded.nickel_source)
            .expect("export harness source");
    assert!(exported.pointer("/roles/smart/systemPrompt").is_none());
    assert!(exported.pointer("/roles/foreman/systemPrompt").is_none());
}

#[test]
fn harness_settings_built_in_gpt_tools_profile() {
    let s = HarnessSettings::built_in();
    assert!(s.tools_profiles["gpt"]["apply_patch"]);
    assert!(!s.tools_profiles["gpt"]["edit"]);
    assert!(!s.tools_profiles["gpt"]["find"]);
    assert!(s.tools_profiles["gpt"]["gpt_shell"]);
    assert!(!s.tools_profiles["gpt"]["grep"]);
    assert!(!s.tools_profiles["gpt"]["ls"]);
    assert!(!s.tools_profiles["gpt"]["read"]);
    assert!(!s.tools_profiles["gpt"]["shell"]);
    assert!(!s.tools_profiles["gpt"]["write"]);
}

#[test]
fn harness_extension_partial_override_keeps_built_in_entry_fields() {
    // Built-in extensions must be merged as ordinary records with defaults on
    // their leaves. If the whole `extensions` map or a built-in entry is
    // defaulted, this user override would erase sibling built-ins or the
    // shell entry's suffix/role fields.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                extensions = {
                    "core-shell" = { enable = false },
                },
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.extensions.contains_key("provider-openai"));
    assert!(s.extensions.contains_key("core-delegate"));
    assert!(s.extensions.contains_key("std-notifications"));

    let shell = s.extensions.get("core-shell").expect("core-shell");
    assert_eq!(shell.enable, Some(false));
    assert_eq!(shell.role.as_deref(), Some("tool"));
    assert_eq!(
        shell.suffix.as_deref(),
        Some(&["ext".to_owned(), "ext-shell".to_owned()][..])
    );
}

#[test]
fn harness_extension_partial_config_override_keeps_built_in_config_defaults() {
    // Extension config defaults must be attached to config's nested leaves. If
    // the entire config record is defaulted, setting only idle_seconds would
    // replace the std-notifications config and lose idle_agent_summary.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                extensions = {
                    "std-notifications" = { config = { idle_seconds = 30 } },
                },
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let config = s.extensions["std-notifications"]
        .config
        .as_ref()
        .expect("std-notifications config");
    assert_eq!(cbor_int_field(config, "idle_seconds"), Some(30));
    assert_eq!(cbor_bool_field(config, "idle_agent_summary"), Some(false));
}

#[test]
fn harness_extension_custom_entry_does_not_erase_built_ins() {
    // Adding one custom extension is an additive map merge; it must not replace
    // the shipped built-in extension table.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                extensions = {
                    mything = { command = ["/usr/local/bin/mything"] },
                },
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.extensions.contains_key("provider-openai"));
    assert!(s.extensions.contains_key("core-shell"));
    assert!(s.extensions.contains_key("core-delegate"));
    assert!(s.extensions.contains_key("std-websearch-exa"));
    assert_eq!(
        s.extensions["mything"].command.as_deref(),
        Some(&["/usr/local/bin/mything".to_owned()][..])
    );
}

#[test]
fn harness_settings_load_tools_profiles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                toolsProfiles = {
                    gpt = {
                        edit = true,
                    },
                    read_only = {
                        shell = false,
                        write = false,
                    },
                },
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.tools_profiles["read_only"]["shell"]);
    assert!(!s.tools_profiles["read_only"]["write"]);
    assert!(s.tools_profiles["gpt"]["apply_patch"]);
    assert!(s.tools_profiles["gpt"]["edit"]);
    assert!(!s.tools_profiles["gpt"]["find"]);
    assert!(s.tools_profiles["gpt"]["gpt_shell"]);
    assert!(!s.tools_profiles["gpt"]["grep"]);
    assert!(!s.tools_profiles["gpt"]["ls"]);
    assert!(!s.tools_profiles["gpt"]["read"]);
    assert!(!s.tools_profiles["gpt"]["shell"]);
    assert!(!s.tools_profiles["gpt"]["write"]);
}

#[test]
fn harness_custom_tools_profile_does_not_erase_built_ins() {
    // The toolsProfiles map is additive, just like roles and extensions.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                toolsProfiles = {
                    read_only = {
                        shell = false,
                        write = false,
                    },
                },
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.tools_profiles.contains_key("read_only"));
    assert!(s.tools_profiles["gpt"]["apply_patch"]);
    assert!(s.tools_profiles["gpt"]["gpt_shell"]);
}

#[test]
fn harness_custom_role_does_not_erase_built_ins() {
    // A user-provided roles map should add entries without replacing the
    // built-in role table.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
                roles = {
                    reviewer = { description = "Focused reviewer", effort = "medium" },
                },
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("smart"));
    assert!(s.roles.contains_key("deep"));
    assert!(s.roles.contains_key("rush"));
    assert!(s.roles.contains_key("foreman"));
    assert_eq!(
        s.roles["reviewer"].description.as_deref(),
        Some("Focused reviewer")
    );
    assert_eq!(s.roles["reviewer"].effort, Some(tau_proto::Effort::Medium));
}

#[test]
fn cli_settings_drop_in_layers_on_top_of_base() {
    // Drop-ins are layered with Nickel's native `&` merge. To make a later
    // layer an override, the lower-priority layer must mark the field as a
    // default; otherwise Nickel correctly reports conflicting non-defaults.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.ncl"), r#"{ greeting | default = true }"#).expect("write");
    std::fs::create_dir(dir.join("cli.d")).expect("mkdir");
    std::fs::write(
        dir.join("cli.d").join("01-override.ncl"),
        r#"{ greeting = false }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
}

#[test]
fn cli_settings_conflicting_non_default_layers_error() {
    // This documents the Nickel-native merge contract: tau no longer performs a
    // custom "later JSON object wins" merge, so two unequal plain values at the
    // same priority are a configuration error instead of an implicit override.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.ncl"), r#"{ greeting = true }"#).expect("write");
    std::fs::create_dir(dir.join("cli.d")).expect("mkdir");
    std::fs::write(
        dir.join("cli.d").join("01-conflict.ncl"),
        r#"{ greeting = false }"#,
    )
    .expect("write");

    let err = load_cli_settings_in(&dirs_with_config(dir)).expect_err("conflict");
    assert!(
        err.to_string().contains("non mergeable terms"),
        "unexpected error: {err}"
    );
}

#[test]
fn harness_roles_merge_with_built_ins() {
    // Roles are harness-owned now. This keeps the old merge behavior while
    // locking the source of truth to harness.ncl instead of a model registry.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
            roles = {
                smart = { model = "openai/gpt-5.5", toolsProfile = "full" },
                custom = { description = "Custom local role", effort = "medium", toolsProfile = "read_only" },
                deep = { model = "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("smart"));
    assert!(s.roles.contains_key("deep"));
    assert!(s.roles.contains_key("rush"));
    assert!(s.roles.contains_key("foreman"));
    assert!(!s.roles.contains_key("default"));
    assert_eq!(
        s.roles["custom"].description.as_deref(),
        Some("Custom local role")
    );
    assert_eq!(s.roles["custom"].effort, Some(tau_proto::Effort::Medium));
    assert_eq!(
        s.roles["custom"].tools_profile.as_deref(),
        Some("read_only")
    );
    assert_eq!(
        s.roles["smart"]
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(s.roles["smart"].tools_profile.as_deref(), Some("full"));

    let deep = &s.roles["deep"];
    assert_eq!(
        deep.description.as_deref(),
        Some(
            "Deep reasoning expert, using potentially slower and more expensive model. Good for research and very complex tasks."
        )
    );
    assert_eq!(
        deep.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(deep.effort, Some(tau_proto::Effort::XHigh));
    assert_eq!(deep.verbosity, None);
    assert_eq!(
        deep.thinking_summary,
        Some(tau_proto::ThinkingSummary::Detailed)
    );
    let foreman = &s.roles["foreman"];
    assert_eq!(
        foreman.description.as_deref(),
        Some("Role focused on splitting and delegation of tasks to other sub-agents")
    );
}

#[test]
fn harness_foreman_partial_override_keeps_built_in_role_metadata() {
    // Foreman delegation instructions are rendered by the harness, not stored
    // in Nickel role fields. Partial role overrides only affect role metadata
    // such as the selected model.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
            roles = {
                foreman = { model = "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let foreman = &s.roles["foreman"];
    assert_eq!(
        foreman.description.as_deref(),
        Some("Role focused on splitting and delegation of tasks to other sub-agents")
    );
    assert_eq!(
        foreman.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
}

#[test]
fn harness_built_in_roles_load_from_nickel_without_prompt_fields() {
    // Built-in role defaults live in built-in.harness.ncl, but prompt behavior
    // is intentionally not part of exported role config anymore.
    let s = HarnessSettings::built_in();
    assert!(s.roles.contains_key("smart"));
    assert!(s.roles.contains_key("deep"));
    assert!(s.roles.contains_key("rush"));
    assert!(s.roles.contains_key("foreman"));
}

#[test]
fn harness_default_roles_alias_still_loads() {
    // Keep the previous `defaultRoles` spelling as a compatibility alias now
    // that roles are loaded from harness config.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.ncl"),
        r#"{
            defaultRoles = {
                custom = { effort = "medium", toolsProfile = "read_only" },
                foreman = { model = "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.roles["custom"].effort, Some(tau_proto::Effort::Medium));
    assert_eq!(
        s.roles["custom"].tools_profile.as_deref(),
        Some("read_only")
    );
    let foreman = &s.roles["foreman"];
    assert_eq!(
        foreman.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
}

#[test]
fn missing_user_files_load_the_built_in_baseline() {
    // With no user files present, the loader still returns fully populated
    // settings from the embedded built-in layer plus harness-owned role/tool
    // defaults. There is intentionally no model registry baseline anymore.
    let td = TempDir::new().expect("tempdir");
    let _cli = load_cli_settings_in(&dirs_with_config(td.path())).expect("cli");
    let harness = load_harness_settings_in(&dirs_with_config(td.path())).expect("harness");
    assert!(harness.roles.contains_key("smart"));
    assert!(harness.roles.contains_key("deep"));
    assert!(harness.roles.contains_key("rush"));
    assert!(harness.roles.contains_key("foreman"));
    assert!(harness.tools_profiles.contains_key("gpt"));
}

#[test]
fn sample_configs_deserialize() {
    // Sanity-check the sample configs shipped in the workspace root `config/`
    // directory (used by `tau init`) by feeding them through the user-config
    // loader. In particular, the sample harness config intentionally uses plain
    // Nickel values and relies on built-in schema/default metadata underneath.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    std::fs::write(
        dir.join("cli.ncl"),
        include_str!("../../../../config/cli.ncl"),
    )
    .expect("write cli");
    std::fs::write(
        dir.join("harness.ncl"),
        include_str!("../../../../config/harness.ncl"),
    )
    .expect("write harness");

    let _cli = load_cli_settings_in(&dirs_with_config(dir)).expect("cli sample should parse");
    let _harness =
        load_harness_settings_in(&dirs_with_config(dir)).expect("harness sample should parse");
}
