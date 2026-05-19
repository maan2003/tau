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
    std::fs::write(dir.join("cli.yaml"), r#"{ greeting: false }"#).expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
}

#[test]
fn cli_settings_user_binding_keeps_built_in_chords() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.yaml"),
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
    assert!(s.bind.contains_key("C-t"));
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
        dir.join("harness.yaml"),
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
fn harness_settings_load_role_tool_lists() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roles: {
                smart: { tools: ["read", "grep"], disableTools: ["grep"] },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.roles["smart"].tools.as_ref().expect("tools"),
        &vec![
            tau_proto::ToolName::new("read"),
            tau_proto::ToolName::new("grep")
        ]
    );
    assert_eq!(
        s.roles["smart"].disable_tools,
        vec![tau_proto::ToolName::new("grep")]
    );
}

#[test]
fn cli_settings_drop_in_layers_on_top_of_base() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), r#"{ greeting: true }"#).expect("write");
    std::fs::create_dir(dir.join("cli.d")).expect("mkdir");
    std::fs::write(
        dir.join("cli.d").join("01-override.yaml"),
        r#"{ greeting: false }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
}

#[test]
fn harness_roles_merge_with_built_ins() {
    // Roles are harness-owned now. This keeps the old merge behavior while
    // locking the source of truth to harness.yaml instead of a model registry.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roles: {
                smart: { model: "openai/gpt-5.5", tools: ["read"] },
                custom: { description: "Custom local role", effort: "medium", disableTools: ["shell"] },
                deep: { model: "openai/gpt-5.5" },
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
        s.roles["custom"].disable_tools,
        vec![tau_proto::ToolName::new("shell")]
    );
    assert_eq!(
        s.roles["smart"]
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(
        s.roles["smart"].tools,
        Some(vec![tau_proto::ToolName::new("read")])
    );

    let deep = &s.roles["deep"];
    assert_eq!(
        deep.description.as_deref(),
        Some(
            "Deep reasoning expert, using potentially slower and more expensive model. Good for research and very complext tasks."
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
    assert!(
        foreman
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str().contains("use the `delegate` tool"))
    );
}

#[test]
fn harness_foreman_partial_override_keeps_built_in_prompt_fragments() {
    // Built-in foreman prompt fragments are stored in the built-in harness
    // config, so a user can partially override foreman settings without
    // accidentally disabling delegation prompt behavior.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roles: {
                foreman: { model: "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let foreman = &s.roles["foreman"];
    assert!(foreman.prompt_fragments.iter().any(|fragment| {
        fragment
            .text
            .as_str()
            .contains("self-contained instructions")
    }));
    assert_eq!(
        foreman.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
}

#[test]
fn harness_foreman_prompt_override_replaces_built_in_prompt() {
    // User-provided role prompt fragments replace the built-in role fragments.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roles: {
                foreman: { promptFragments: [{ name: "foreman.custom", priority: 100, text: "Custom foreman prompt." }] },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let foreman = &s.roles["foreman"];
    assert_eq!(
        foreman
            .prompt_fragments
            .first()
            .map(|fragment| fragment.text.as_str()),
        Some("Custom foreman prompt.")
    );
}

#[test]
fn harness_role_prompt_fragments_parse_as_plain_strings() {
    // Role prompt customization must keep harness.yaml ergonomic: users write
    // prompt text directly instead of nested newtype objects.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roles: {
                custom: {
                    promptFragments: [
                        { name: "custom.reviewer", priority: 100, text: "You are a focused reviewer." },
                        { name: "custom.patch-style", priority: 200, text: "Prefer small patches." },
                    ],
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let role = &s.roles["custom"];
    assert_eq!(
        role.prompt_fragments
            .first()
            .map(|fragment| fragment.text.as_str()),
        Some("You are a focused reviewer.")
    );
    assert_eq!(
        role.prompt_fragments
            .get(1)
            .map(|fragment| fragment.text.as_str()),
        Some("Prefer small patches.")
    );
}

#[test]
fn harness_built_in_roles_load_from_json_with_foreman_prompt() {
    // Built-in role defaults live in built-in.harness.yaml. Foreman has a
    // visible built-in prompt there; the individual-contributor roles do not.
    let s = HarnessSettings::built_in();
    assert!(s.roles["smart"].prompt_fragments.is_empty());
    assert!(s.roles["deep"].prompt_fragments.is_empty());
    assert!(s.roles["rush"].prompt_fragments.is_empty());
    let foreman = &s.roles["foreman"];
    let prompt = foreman
        .prompt_fragments
        .first()
        .expect("foreman prompt fragment")
        .text
        .as_str();
    assert!(prompt.contains("You are a foreman/orchestrator agent"));
    assert!(prompt.contains("use the `delegate` tool"));
    assert!(prompt.contains("available sub-task roles list"));
}

#[test]
fn harness_default_roles_alias_still_loads() {
    // Keep the previous `defaultRoles` spelling as a compatibility alias now
    // that roles are loaded from harness config.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            defaultRoles: {
                custom: { effort: "medium", tools: ["read"] },
                foreman: { model: "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.roles["custom"].effort, Some(tau_proto::Effort::Medium));
    assert_eq!(
        s.roles["custom"].tools.as_ref().expect("tools"),
        &vec![tau_proto::ToolName::new("read")]
    );
    let foreman = &s.roles["foreman"];
    assert!(
        foreman
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str().contains("use the `delegate` tool"))
    );
    assert_eq!(
        foreman.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
}

#[test]
fn missing_user_files_load_the_built_in_baseline() {
    // With no user files present, the loader still returns fully populated
    // settings from the embedded built-in layer plus harness-owned role defaults.
    // There is intentionally no model registry baseline anymore.
    let td = TempDir::new().expect("tempdir");
    let _cli = load_cli_settings_in(&dirs_with_config(td.path())).expect("cli");
    let harness = load_harness_settings_in(&dirs_with_config(td.path())).expect("harness");
    assert!(harness.roles.contains_key("smart"));
    assert!(harness.roles.contains_key("deep"));
    assert!(harness.roles.contains_key("rush"));
    assert!(harness.roles.contains_key("foreman"));
}

#[test]
fn sample_configs_deserialize() {
    // Sanity-check the sample configs shipped in the workspace root `config/`
    // directory (used by `tau init`) by feeding them through the user-config
    // loader.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    std::fs::write(
        dir.join("cli.yaml"),
        include_str!("../../../../config/cli.yaml"),
    )
    .expect("write cli");
    std::fs::write(
        dir.join("harness.yaml"),
        include_str!("../../../../config/harness.yaml"),
    )
    .expect("write harness");

    let _cli = load_cli_settings_in(&dirs_with_config(dir)).expect("cli sample should parse");
    let _harness =
        load_harness_settings_in(&dirs_with_config(dir)).expect("harness sample should parse");
}
