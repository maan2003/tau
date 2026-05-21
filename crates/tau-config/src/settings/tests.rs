use tempfile::TempDir;

use super::*;

fn dirs_with_config(dir: &std::path::Path) -> TauDirs {
    TauDirs {
        config_dir: Some(dir.to_path_buf()),
        state_dir: None,
    }
}

fn dirs_with_config_and_state(
    config_dir: &std::path::Path,
    state_dir: &std::path::Path,
) -> TauDirs {
    TauDirs {
        config_dir: Some(config_dir.to_path_buf()),
        state_dir: Some(state_dir.to_path_buf()),
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
    std::fs::write(
        dir.join("cli.yaml"),
        r#"{ greeting: false, show_thinking: false, show_tools: "compact" }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
    assert!(!s.show_thinking);
    assert_eq!(s.show_tools, ShowTools::Compact);
    assert_eq!(s.theme, CliTheme::Auto);
}

#[test]
fn cli_settings_theme_override() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), r#"{ theme: "light" }"#).expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.theme, CliTheme::Light);
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
    let cr = s.bind.get("C-r").expect("C-r");
    assert_eq!(cr.action, "prompt-history-search");
    assert!(cr.trim);
    assert!(
        cr.command
            .as_deref()
            .is_some_and(|command| command.contains("fzf"))
    );
    assert!(s.bind.contains_key("C-t"));
    assert!(s.bind.contains_key("C-o"));
    assert_eq!(
        s.bind.get("C-Enter").expect("C-Enter").action,
        "submit-prompt"
    );
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
        show_turn_stats: true,
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
fn cli_state_defaults_to_cli_config_when_state_file_is_missing() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("cli.yaml"),
        r#"{ show_diff: true, show_thinking: false, show_turn_stats: true, redraw_counter: true, show_tools: "compact" }"#,
    )
    .expect("write");

    let dirs = dirs_with_config_and_state(&config_dir, &state_dir);
    let settings = load_cli_settings_in(&dirs).expect("load settings");
    let state = CliState::load_with_default(&dirs, settings.default_state());

    assert_eq!(
        state,
        CliState {
            show_diff: true,
            show_thinking: false,
            show_turn_stats: true,
            redraw_counter: true,
            show_tools: ShowTools::Compact,
        }
    );
}

#[test]
fn cli_state_file_overrides_cli_config_defaults() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(config_dir.join("cli.yaml"), r#"{ show_thinking: false }"#).expect("write");
    std::fs::write(state_dir.join("cli.json"), r#"{"show_thinking":true}"#).expect("write");

    let dirs = dirs_with_config_and_state(&config_dir, &state_dir);
    let settings = load_cli_settings_in(&dirs).expect("load settings");
    let state = CliState::load_with_default(&dirs, settings.default_state());

    assert!(state.show_thinking);
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
                engineer: { tools: ["read", "grep"], disableTools: ["grep"] },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.roles["engineer"].tools.as_ref().expect("tools"),
        &vec![
            tau_proto::ToolName::new("read"),
            tau_proto::ToolName::new("grep")
        ]
    );
    assert_eq!(
        s.roles["engineer"].disable_tools,
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
fn harness_drop_in_layers_merge_through_domain_overrides() {
    // Harness files are applied as sparse overrides one layer at a time. This
    // keeps role prompt fragments additive across the built-in baseline,
    // harness.yaml, and harness.d/*.yaml instead of letting generic YAML array
    // replacement discard earlier fragments before role merging can run.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            session_retention_days: 7,
            extensions: {
                mything: { command: ["mything"] },
            },
            promptFragments: [
                { name: "global.local", priority: 60, text: "Local global instruction." },
            ],
            roles: {
                manager: { promptFragments: [{ name: "manager.local", priority: 170, text: "Local manager instruction." }] },
            },
        }"#,
    )
    .expect("write harness");
    std::fs::create_dir(dir.join("harness.d")).expect("mkdir harness.d");
    std::fs::write(
        dir.join("harness.d").join("01-extra.yaml"),
        r#"{
            session_retention_days: 14,
            extensions: {
                mything: { suffix: ["--flag"] },
            },
            promptFragments: [
                { name: "global.drop-in", priority: 70, text: "Drop-in global instruction." },
            ],
            roles: {
                manager: { promptFragments: [{ name: "manager.drop-in", priority: 180, text: "Drop-in manager instruction." }] },
            },
        }"#,
    )
    .expect("write drop-in");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.session_retention_days, 14);
    assert_eq!(
        s.extensions["mything"].command.as_ref().expect("command"),
        &vec!["mything".to_owned()]
    );
    assert_eq!(
        s.extensions["mything"].suffix.as_ref().expect("suffix"),
        &vec!["--flag".to_owned()]
    );
    assert!(
        s.prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Local global instruction.")
    );
    assert!(
        s.prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Drop-in global instruction.")
    );
    let manager = &s.roles["manager"];
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| { fragment.text.as_str().contains("delegating to sub-agents") })
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Local manager instruction.")
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Drop-in manager instruction.")
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Local global instruction.")
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Drop-in global instruction.")
    );
}

#[test]
fn harness_global_prompt_fragments_apply_to_all_roles() {
    // Top-level prompt fragments are role-independent style/context hooks. They
    // must apply to built-in roles and roles created by user config without
    // duplicating the same fragment when a drop-in repeats it exactly.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            promptFragments: [
                { name: "global.simple", priority: 65, text: "Use simple words." },
            ],
            roles: {
                custom: { model: "openai/custom" },
            },
        }"#,
    )
    .expect("write harness");
    std::fs::create_dir(dir.join("harness.d")).expect("mkdir harness.d");
    std::fs::write(
        dir.join("harness.d").join("01-repeat.yaml"),
        r#"{
            promptFragments: [
                { name: "global.simple", priority: 65, text: "Use simple words." },
            ],
        }"#,
    )
    .expect("write drop-in");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.prompt_fragments
            .iter()
            .filter(|fragment| fragment.name == "global.simple")
            .count(),
        1
    );
    for role_name in ["assistant", "engineer", "manager", "custom"] {
        let role = &s.roles[role_name];
        assert_eq!(
            role.prompt_fragments
                .iter()
                .filter(|fragment| fragment.name == "global.simple")
                .count(),
            1,
            "global fragment should apply once to {role_name}"
        );
    }
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
                engineer: { model: "openai/gpt-5.5", tools: ["read"] },
                custom: { description: "Custom local role", effort: "medium", disableTools: ["shell"] },
                manager: { model: "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("engineer"));
    assert!(s.roles.contains_key("manager"));
    assert!(s.roles.contains_key("assistant"));
    assert!(!s.roles.contains_key("smart"));
    assert!(!s.roles.contains_key("deep"));
    assert!(!s.roles.contains_key("rush"));
    assert!(!s.roles.contains_key("foreman"));
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
        s.roles["engineer"]
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(
        s.roles["engineer"].tools,
        Some(vec![tau_proto::ToolName::new("read")])
    );

    let assistant = &s.roles["assistant"];
    assert_eq!(
        assistant.description.as_deref(),
        Some("Fast and lightweight assistant.")
    );
    assert_eq!(
        assistant.model.as_ref().map(ToString::to_string).as_deref(),
        None
    );
    assert_eq!(assistant.effort, Some(tau_proto::Effort::Off));
    assert_eq!(assistant.verbosity, None);
    assert_eq!(assistant.thinking_summary, None);
    let manager = &s.roles["manager"];
    assert_eq!(
        manager.description.as_deref(),
        Some("Role focused on splitting and delegation of tasks to other sub-agents.")
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str().contains("delegating to sub-agents"))
    );
}

#[test]
fn harness_manager_partial_override_keeps_built_in_prompt_fragments() {
    // Built-in manager prompt fragments are stored in the built-in harness
    // config, so a user can partially override manager settings without
    // accidentally disabling delegation prompt behavior.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roles: {
                manager: { model: "openai/gpt-5.5" },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let manager = &s.roles["manager"];
    assert!(manager.prompt_fragments.iter().any(|fragment| {
        fragment
            .text
            .as_str()
            .contains("self-contained instructions")
    }));
    assert_eq!(
        manager.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
}

#[test]
fn harness_manager_prompt_fragments_extend_built_in_prompt_fragments() {
    // User-provided role prompt fragments are added to the built-in role
    // fragments so partial manager customization does not disable delegation
    // instructions.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roles: {
                manager: { promptFragments: [{ name: "manager.custom", priority: 100, text: "Custom manager prompt." }] },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let manager = &s.roles["manager"];
    assert!(manager.prompt_fragments.iter().any(|fragment| {
        fragment
            .text
            .as_str()
            .contains("self-contained instructions")
    }));
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Custom manager prompt.")
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
fn harness_built_in_roles_load_from_json_with_manager_prompt() {
    // Built-in role defaults live in built-in.harness.yaml. Manager has a
    // visible built-in prompt there; the individual-contributor roles do not.
    let s = HarnessSettings::built_in();
    assert!(s.roles["engineer"].prompt_fragments.is_empty());
    assert!(s.roles["assistant"].prompt_fragments.is_empty());
    let manager = &s.roles["manager"];
    let prompt = manager
        .prompt_fragments
        .first()
        .expect("manager prompt fragment")
        .text
        .as_str();
    assert_eq!(manager.prompt_fragments[0].priority, PromptPriority::new(5));
    assert_eq!(manager.prompt_fragments[1].priority, PromptPriority::new(6));
    assert!(prompt.contains("You are a planning and orchestration agent"));
    assert!(prompt.contains("delegating to sub-agents"));
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
                manager: { model: "openai/gpt-5.5" },
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
    let manager = &s.roles["manager"];
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str().contains("delegating to sub-agents"))
    );
    assert_eq!(
        manager.model.as_ref().map(ToString::to_string).as_deref(),
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
    assert!(harness.roles.contains_key("engineer"));
    assert!(harness.roles.contains_key("manager"));
    assert!(harness.roles.contains_key("assistant"));
    assert!(!harness.roles.contains_key("smart"));
    assert!(!harness.roles.contains_key("deep"));
    assert!(!harness.roles.contains_key("rush"));
    assert!(!harness.roles.contains_key("foreman"));
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
