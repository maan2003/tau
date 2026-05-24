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
        r#"{ greeting: false, show_thinking: false, show_tools: "compact", show_messages: "self-summary" }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
    assert!(!s.show_thinking);
    assert_eq!(s.show_tools, ShowTools::Compact);
    assert_eq!(s.show_messages, ShowMessages::SelfSummary);
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
    assert_eq!(
        s.bind.get("BackTab").expect("BackTab").action,
        "cycle-role-group"
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
        show_messages: crate::settings::ShowMessages::AllSummary,
    };
    original.save(&dirs);
    assert!(td.path().join("cli.json").exists());
    let reloaded = CliState::load(&dirs);
    assert_eq!(reloaded, original);
}

#[test]
fn cli_state_defaults_missing_show_messages_to_all_full() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    std::fs::write(td.path().join("cli.json"), r#"{"show_tools":"compact"}"#).expect("write");

    let loaded = CliState::load(&dirs);
    assert_eq!(loaded.show_messages, crate::settings::ShowMessages::AllFull);
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
        r#"{ show_diff: true, show_thinking: false, show_turn_stats: true, redraw_counter: true, show_tools: "compact", show_messages: "self-full" }"#,
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
            show_messages: ShowMessages::SelfFull,
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
            roleGroups: {
                engineer: {
                    roles: {
                        engineer: { tools: ["read", "grep"], disableTools: ["grep"] },
                    },
                },
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
fn harness_settings_load_role_compaction_threshold() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                engineer: {
                    compactionThreshold: 70,
                    roles: {
                        engineer: { compactionThreshold: 80 },
                        reviewer: {},
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.roles["engineer"].compaction_threshold, Some(80));
    assert_eq!(s.roles["reviewer"].compaction_threshold, Some(70));
}

#[test]
fn harness_settings_rejects_invalid_role_compaction_threshold() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                engineer: {
                    roles: {
                        engineer: { compactionThreshold: 101 },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir))
        .expect_err("reject invalid compaction threshold");
    assert!(
        error.to_string().contains("percentage from 0 to 100"),
        "error should mention valid percentage range: {error}"
    );
}

#[test]
fn harness_settings_load_role_group_default_tool_overrides_without_relisting_roles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                engineer: { disableTools: ["email"] },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    for role_name in ["senior-engineer", "junior-engineer", "staff-engineer"] {
        assert_eq!(
            s.roles[role_name].disable_tools,
            vec![tau_proto::ToolName::new("email")]
        );
    }
}

#[test]
fn harness_settings_allow_new_role_group() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                reviewers: {
                    disableTools: ["email"],
                    roles: {
                        reviewer: { effort: "high" },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.role_groups.last().expect("new group").name, "reviewers");
    assert_eq!(
        s.roles["reviewer"].disable_tools,
        vec![tau_proto::ToolName::new("email")]
    );
}

#[test]
fn harness_settings_rejects_role_in_multiple_groups() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                reviewers: {
                    roles: {
                        senior-engineer: { effort: "high" },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let error =
        load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject duplicate role");
    assert!(
        error
            .to_string()
            .contains("role `senior-engineer` appears in multiple roleGroups"),
        "error should mention duplicate role: {error}"
    );
}

#[test]
fn harness_settings_rejects_unknown_top_level_fields() {
    // Unknown harness.yaml keys used to be silently ignored. That hides stale
    // configs after refactors, so loading must fail and let the harness print a
    // loud startup warning instead.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("harness.yaml"), r#"{ staleThing: true }"#).expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject unknown field");
    assert!(
        error.to_string().contains("staleThing"),
        "error should mention unknown field: {error}"
    );
}

#[test]
fn harness_settings_rejects_unknown_role_fields() {
    // Role entries are nested under arbitrary group and role names, so strict
    // parsing has to happen at the AgentRole level too.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                engineer: {
                    roles: {
                        senior-engineer: { staleRoleField: true },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let error =
        load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject unknown role field");
    assert!(
        error.to_string().contains("staleRoleField"),
        "error should mention unknown role field: {error}"
    );
}

#[test]
fn harness_settings_rejects_unknown_prompt_fragment_fields() {
    // Prompt fragments are user-authored config too; typos there must not be
    // accepted as no-ops.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            promptFragments: [
                { name: "global.typo", priority: 50, text: "x", staleFragmentField: true },
            ],
        }"#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir))
        .expect_err("reject unknown fragment field");
    assert!(
        error.to_string().contains("staleFragmentField"),
        "error should mention unknown fragment field: {error}"
    );
}

#[test]
fn harness_settings_role_cli_overrides_apply_in_order_after_config() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                manager: {
                    roles: {
                        manager: { enabled: false },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_with_role_overrides_in(
        &dirs_with_config(dir),
        &[
            RoleCliOverride::DisableAll,
            RoleCliOverride::Enable("manager".to_owned()),
        ],
    )
    .expect("load");

    assert_eq!(s.roles.keys().collect::<Vec<_>>(), vec!["manager"]);
    assert_eq!(s.role_groups.len(), 1);
    assert_eq!(s.role_groups[0].name, "manager");
    assert_eq!(s.role_groups[0].roles, vec!["manager"]);
}

#[test]
fn harness_settings_role_cli_overrides_later_disable_wins() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    let s = load_harness_settings_with_role_overrides_in(
        &dirs_with_config(dir),
        &[
            RoleCliOverride::Enable("manager".to_owned()),
            RoleCliOverride::Disable("manager".to_owned()),
        ],
    )
    .expect("load");

    assert!(!s.roles.contains_key("manager"));
}

#[test]
fn harness_settings_role_cli_disable_all_leaves_no_effective_roles() {
    // `--disable-roles-all` must not be undone by default-role fallback. The
    // harness reports an explicit startup error for this empty effective role set.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    let s = load_harness_settings_with_role_overrides_in(
        &dirs_with_config(dir),
        &[RoleCliOverride::DisableAll],
    )
    .expect("load");

    assert!(s.roles.is_empty());
    assert!(s.role_groups.is_empty());
    assert_eq!(s.default_role.as_deref(), Some("senior-engineer"));
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
            roleGroups: {
                manager: {
                    roles: {
                        manager: { promptFragments: [{ name: "manager.local", priority: 170, text: "Local manager instruction." }] },
                    },
                },
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
            roleGroups: {
                manager: {
                    roles: {
                        manager: { promptFragments: [{ name: "manager.drop-in", priority: 180, text: "Drop-in manager instruction." }] },
                    },
                },
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
            roleGroups: {
                custom: {
                    roles: {
                        custom: { model: "openai/custom" },
                    },
                },
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
    for role_name in ["senior-engineer", "manager", "custom"] {
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
            roleGroups: {
                engineer: {
                    roles: {
                        engineer: { model: "openai/gpt-5.5", tools: ["read"] },
                        custom: { description: "Custom local role", effort: "medium", disableTools: ["shell"] },
                    },
                },
                manager: {
                    roles: {
                        manager: { model: "openai/gpt-5.5" },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("engineer"));
    assert!(s.roles.contains_key("manager"));
    assert!(!s.roles.contains_key("assistant"));
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
            roleGroups: {
                manager: {
                    roles: {
                        manager: { model: "openai/gpt-5.5" },
                    },
                },
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
            roleGroups: {
                manager: {
                    roles: {
                        manager: { promptFragments: [{ name: "manager.custom", priority: 100, text: "Custom manager prompt." }] },
                    },
                },
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
fn harness_role_group_fields_apply_as_role_defaults() {
    // Group-level role fields keep shared role policy in one place. Individual
    // roles can still override scalar defaults or add their own fragments.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                review: {
                    effort: "low",
                    tools: ["read"],
                    promptFragments: [
                        { name: "review.shared", priority: 80, text: "Review carefully." },
                    ],
                    roles: {
                        quick: {},
                        deep: {
                            effort: "xhigh",
                            promptFragments: [
                                { name: "review.deep", priority: 90, text: "Look for subtle issues." },
                            ],
                        },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let quick = &s.roles["quick"];
    assert_eq!(quick.effort, Some(tau_proto::Effort::Low));
    assert_eq!(quick.tools, Some(vec![tau_proto::ToolName::new("read")]));
    assert!(
        quick
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.name == "review.shared")
    );

    let deep = &s.roles["deep"];
    assert_eq!(deep.effort, Some(tau_proto::Effort::XHigh));
    assert!(
        deep.prompt_fragments
            .iter()
            .any(|fragment| fragment.name == "review.shared")
    );
    assert!(
        deep.prompt_fragments
            .iter()
            .any(|fragment| fragment.name == "review.deep")
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
            roleGroups: {
                review: {
                    roles: {
                        custom: {
                            promptFragments: [
                                { name: "custom.reviewer", priority: 100, text: "You are a focused reviewer." },
                                { name: "custom.patch-style", priority: 200, text: "Prefer small patches." },
                            ],
                        },
                    },
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
    // visible orchestration prompt there. Engineer roles have a lightweight
    // follow-up prompt for delegated tasks.
    let s = HarnessSettings::built_in();
    assert_eq!(s.default_role.as_deref(), Some("senior-engineer"));
    assert_eq!(
        s.role_groups
            .iter()
            .map(|group| (group.name.clone(), group.roles.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                "engineer".to_owned(),
                vec![
                    "senior-engineer".to_owned(),
                    "junior-engineer".to_owned(),
                    "staff-engineer".to_owned(),
                ],
            ),
            ("manager".to_owned(), vec!["manager".to_owned()]),
        ]
    );
    let junior_engineer = &s.roles["junior-engineer"];
    assert_eq!(junior_engineer.effort, Some(tau_proto::Effort::Low));
    let senior_engineer = &s.roles["senior-engineer"];
    assert_eq!(
        senior_engineer.prompt_fragments[0].priority,
        PromptPriority::new(5)
    );
    assert!(
        senior_engineer.prompt_fragments[0]
            .text
            .contains("Trust the `<instructions>`")
    );
    assert!(!s.roles.contains_key("assistant"));
    let staff_engineer = &s.roles["staff-engineer"];
    assert_eq!(staff_engineer.effort, Some(tau_proto::Effort::XHigh));
    assert!(
        staff_engineer
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.contains("Trust the `<instructions>`"))
    );
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
fn harness_role_groups_load_custom_roles() {
    // Role groups are the user-facing role configuration shape.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                coding: {
                    roles: {
                        custom: { effort: "medium", tools: ["read"] },
                    },
                },
                manager: {
                    roles: {
                        manager: { model: "openai/gpt-5.5" },
                    },
                },
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
fn harness_role_groups_reject_duplicate_role_names() {
    // Role names are runtime identities, so grouping is only navigation; the
    // same role name in two groups would make keyboard traversal ambiguous.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                coding: { roles: { engineer: {} } },
                review: { roles: { engineer: {} } },
            },
        }"#,
    )
    .expect("write");

    let err = load_harness_settings_in(&dirs_with_config(dir)).expect_err("duplicate role");
    assert!(err.to_string().contains("appears in multiple roleGroups"));
}

#[test]
fn missing_user_files_load_the_built_in_baseline() {
    // With no user files present, the loader still returns fully populated
    // settings from the embedded built-in layer plus harness-owned role defaults.
    // There is intentionally no model registry baseline anymore.
    let td = TempDir::new().expect("tempdir");
    let _cli = load_cli_settings_in(&dirs_with_config(td.path())).expect("cli");
    let harness = load_harness_settings_in(&dirs_with_config(td.path())).expect("harness");
    assert!(harness.roles.contains_key("junior-engineer"));
    assert!(harness.roles.contains_key("senior-engineer"));
    assert!(harness.roles.contains_key("manager"));
    assert_eq!(harness.default_role.as_deref(), Some("senior-engineer"));
    assert!(!harness.roles.contains_key("assistant"));
    assert!(harness.roles.contains_key("staff-engineer"));
    assert_eq!(
        harness.roles["staff-engineer"].effort,
        Some(tau_proto::Effort::XHigh)
    );
    assert!(!harness.roles.contains_key("smart"));
    assert!(!harness.roles.contains_key("deep"));
    assert!(!harness.roles.contains_key("rush"));
    assert!(!harness.roles.contains_key("foreman"));
}

#[test]
fn harness_role_enabled_false_filters_built_in_roles_after_merging() {
    // `enabled: false` is the merge-friendly way to remove a role supplied by a
    // lower layer: the role can keep its inherited config shape, but disappears
    // from the effective role map and navigation groups after all layers merge.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            defaultRole: "senior-engineer",
            roleGroups: {
                engineer: {
                    roles: {
                        "junior-engineer": { enabled: false },
                        "senior-engineer": { enabled: false },
                        "staff-engineer": { enabled: false },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.roles.contains_key("junior-engineer"));
    assert!(!s.roles.contains_key("senior-engineer"));
    assert!(!s.roles.contains_key("staff-engineer"));
    assert!(!s.roles.contains_key("assistant"));
    assert_eq!(s.default_role.as_deref(), Some("senior-engineer"));
    assert_eq!(
        s.role_groups
            .iter()
            .map(|group| (group.name.as_str(), group.roles.as_slice()))
            .collect::<Vec<_>>(),
        vec![("manager", &["manager".to_owned()][..]),]
    );
}

#[test]
fn harness_role_enabled_can_be_reenabled_by_later_layers() {
    // Filtering happens after the complete domain merge, so a higher-priority
    // drop-in can re-enable a role disabled by the base user config.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir drop-ins");
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{ roleGroups: { engineer: { roles: { "staff-engineer": { enabled: false } } } } }"#,
    )
    .expect("write base");
    std::fs::write(
        dir.join("harness.d/10-enable.yaml"),
        r#"{ roleGroups: { engineer: { roles: { "staff-engineer": { enabled: true, effort: "xhigh" } } } } }"#,
    )
    .expect("write drop-in");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("staff-engineer"));
    assert_eq!(s.roles["staff-engineer"].enabled, Some(true));
    assert!(
        s.role_groups.iter().any(|group| group.name == "engineer"
            && group.roles.iter().any(|role| role == "staff-engineer"))
    );
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

#[test]
fn extension_state_dir_rejects_unsafe_extension_names() {
    // Extension names can come from user-authored harness.yaml keys. Rejecting
    // anything other than a conservative single path component keeps the
    // injected state directory confined under state/ext/<extension>.
    let state_dir = std::path::Path::new("/tmp/tau-state");
    assert_eq!(
        extension_state_dir_of(state_dir, "std-email").expect("safe extension name"),
        state_dir.join("ext").join("std-email")
    );

    for name in ["", "../x", "a/b", "/tmp/x", ".", ".."] {
        assert!(
            extension_state_dir_of(state_dir, name).is_err(),
            "{name:?} must be rejected"
        );
    }
}

#[test]
fn harness_extension_secrets_parse_with_required_default() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
extensions:
  std-email:
    secrets:
      mail_password: {}
      optional_token:
        optional: true
"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let secrets = s.extensions["std-email"].secrets.as_ref().expect("secrets");
    assert!(!secrets["mail_password"].optional);
    assert!(secrets["optional_token"].optional);
}

#[test]
fn harness_extension_secret_entries_deny_unknown_fields() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
extensions:
  std-email:
    secrets:
      mail_password:
        bogus: true
"#,
    )
    .expect("write");

    let err = load_harness_settings_in(&dirs_with_config(dir)).expect_err("unknown field rejected");
    assert!(err.to_string().contains("bogus"), "unexpected error: {err}");
}
