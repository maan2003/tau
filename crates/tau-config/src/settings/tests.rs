use tempfile::TempDir;

use super::*;

fn dirs_with_config(dir: &std::path::Path) -> TauDirs {
    TauDirs {
        config_dir: Some(dir.to_path_buf()),
        state_dir: None,
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
    std::fs::write(dir.join("cli.json5"), r#"{ greeting: false }"#).expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
}

#[test]
fn cli_settings_user_binding_keeps_built_in_chords() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.json5"),
        r#"{ bind: { "C-f": { action: "shell-prompt-edit", command: "pick", trim: true } } }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    // User-overridden key reflects the user's value...
    let cf = s.bind.get("C-f").expect("C-f");
    assert_eq!(cf.action, "shell-prompt-edit");
    assert_eq!(cf.command.as_deref(), Some("pick"));
    // ...and other built-in chords survive the merge.
    assert!(s.bind.contains_key("C-r"));
    assert!(s.bind.contains_key("C-o"));
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
        dir.join("harness.json5"),
        r#"{
                session_retention_days: 7,
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
fn harness_settings_load_tools_profiles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.json5"),
        r#"{
                toolsProfiles: {
                    gpt: {
                        edit: true,
                    },
                    read_only: {
                        shell: false,
                        write: false,
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
fn cli_settings_drop_in_layers_on_top_of_base() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.json5"), r#"{ greeting: true }"#).expect("write");
    std::fs::create_dir(dir.join("cli.d")).expect("mkdir");
    std::fs::write(
        dir.join("cli.d").join("01-override.json5"),
        r#"{ greeting: false }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
}

#[test]
fn harness_roles_merge_with_built_ins() {
    // Roles are harness-owned now. This keeps the old merge behavior while
    // locking the source of truth to harness.json5 instead of a model registry.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.json5"),
        r#"{
            roles: {
                smart: { model: "openai/gpt-5.5", toolsProfile: "full" },
                custom: { effort: "medium", toolsProfile: "read_only" },
                deep: { model: "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("smart"));
    assert!(s.roles.contains_key("deep"));
    assert!(s.roles.contains_key("rush"));
    assert!(!s.roles.contains_key("default"));
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
        deep.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(deep.effort, Some(tau_proto::Effort::XHigh));
    assert_eq!(deep.verbosity, None);
    assert_eq!(
        deep.thinking_summary,
        Some(tau_proto::ThinkingSummary::Detailed)
    );
}

#[test]
fn harness_default_roles_alias_still_loads() {
    // Keep the previous `defaultRoles` spelling as a compatibility alias now
    // that roles are loaded from harness config.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.json5"),
        r#"{
            defaultRoles: {
                custom: { effort: "medium", toolsProfile: "read_only" },
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
    assert!(harness.tools_profiles.contains_key("gpt"));
}

#[test]
fn sample_configs_deserialize() {
    // Sanity-check the sample configs shipped in the workspace root `config/`
    // directory (used by `tau init`) by feeding them through the user-config
    // loader.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    std::fs::write(
        dir.join("cli.json5"),
        include_str!("../../../../config/cli.json5"),
    )
    .expect("write cli");
    std::fs::write(
        dir.join("harness.json5"),
        include_str!("../../../../config/harness.json5"),
    )
    .expect("write harness");

    let _cli = load_cli_settings_in(&dirs_with_config(dir)).expect("cli sample should parse");
    let _harness =
        load_harness_settings_in(&dirs_with_config(dir)).expect("harness sample should parse");
}
