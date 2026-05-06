use tempfile::TempDir;

use super::*;

#[test]
fn default_cli_settings_have_logo_enabled() {
    let s = CliSettings::default();
    assert!(s.greeting);
    assert!(s.show_logo);
    assert!(s.bar_cursor);
}

#[test]
fn default_harness_settings_have_no_model() {
    let s = HarnessSettings::default();
    assert!(s.default_model.is_none());
    assert!(s.default_efforts.is_empty());
}

#[test]
fn cli_settings_load_from_json5_file() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.json5"), r#"{ greeting: false }"#).expect("write");

    let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
    assert!(!s.greeting);
    assert!(s.show_logo); // default
    assert!(s.bar_cursor); // default
}

#[test]
fn cli_state_defaults_when_file_missing() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    let state = CliState::load(&dirs);
    assert_eq!(state, CliState::default());
    assert!(!state.show_diff);
    assert!(state.show_thinking);
    assert!(state.show_cache_stats);
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
        show_cache_stats: false,
    };
    original.save(&dirs);
    assert!(td.path().join("cli.json").exists());
    let reloaded = CliState::load(&dirs);
    assert_eq!(reloaded, original);
}

#[test]
fn cli_settings_can_disable_bar_cursor() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.json5"), r#"{ bar_cursor: false }"#).expect("write");

    let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
    assert!(!s.bar_cursor);
    assert!(s.greeting); // default
    assert!(s.show_logo); // default
}

#[test]
fn harness_settings_load_from_json5_file() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.json5"),
        r#"{
                default_model: "anthropic/claude-sonnet-4-20250514",
                default_efforts: {
                    "anthropic/claude-sonnet-4-20250514": "high",
                },
            }"#,
    )
    .expect("write");

    let s: HarnessSettings = load_json5_layered(dir, "harness").expect("load");
    assert_eq!(
        s.default_model.as_deref(),
        Some("anthropic/claude-sonnet-4-20250514")
    );
    assert_eq!(
        s.default_efforts
            .get("anthropic/claude-sonnet-4-20250514")
            .copied(),
        Some(tau_proto::Effort::High)
    );
}

fn builtins() -> Vec<BuiltinExtension> {
    vec![
        BuiltinExtension {
            name: "core-agent",
            command: vec!["tau".into(), "ext".into(), "agent".into()],
            role: Some("agent"),
            enable: true,
            config: serde_json::json!({}),
        },
        BuiltinExtension {
            name: "core-shell",
            command: vec!["tau".into(), "ext".into(), "ext-shell".into()],
            role: Some("tool"),
            enable: true,
            config: serde_json::json!({}),
        },
        BuiltinExtension {
            name: "test-dummy",
            command: vec!["tau".into(), "ext".into(), "ext-test-dummy".into()],
            role: Some("tool"),
            enable: false,
            config: serde_json::json!({}),
        },
        BuiltinExtension {
            name: "core-notifications",
            command: vec!["tau".into(), "ext".into(), "ext-core-notifications".into()],
            role: Some("tool"),
            enable: true,
            config: serde_json::json!({ "idle_seconds": 60 }),
        },
    ]
}

#[test]
fn resolve_extensions_returns_builtins_when_user_config_empty() {
    let s = HarnessSettings::default();
    let resolved = s.resolve_extensions(builtins()).expect("resolve");
    assert_eq!(resolved.len(), 3);
    assert_eq!(resolved[0].name, "core-agent");
    assert_eq!(resolved[0].command, "tau");
    assert_eq!(resolved[0].args, vec!["ext", "agent"]);
    assert_eq!(resolved[0].role.as_deref(), Some("agent"));
    assert_eq!(resolved[1].name, "core-shell");
    assert_eq!(resolved[2].name, "core-notifications");
}

#[test]
fn resolve_extensions_builtin_can_start_disabled() {
    let s = HarnessSettings::default();
    let resolved = s.resolve_extensions(builtins()).expect("resolve");
    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
}

#[test]
fn resolve_extensions_disable_drops_entry() {
    let mut s = HarnessSettings::default();
    s.extensions.insert(
        "core-shell".into(),
        ExtensionEntry {
            enable: false,
            ..Default::default()
        },
    );
    let resolved = s.resolve_extensions(builtins()).expect("resolve");
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].name, "core-agent");
    assert_eq!(resolved[1].name, "core-notifications");
}

#[test]
fn resolve_extensions_prefix_wraps_builtin_command() {
    let mut s = HarnessSettings::default();
    s.extensions.insert(
        "core-agent".into(),
        ExtensionEntry {
            prefix: vec!["ssh".into(), "user@host".into()],
            ..Default::default()
        },
    );
    let resolved = s.resolve_extensions(builtins()).expect("resolve");
    let agent = resolved
        .iter()
        .find(|e| e.name == "core-agent")
        .expect("agent");
    // argv[0] is the wrapper; original command moves into args.
    assert_eq!(agent.command, "ssh");
    assert_eq!(agent.args, vec!["user@host", "tau", "ext", "agent"]);
}

#[test]
fn resolve_extensions_user_command_replaces_builtin_command() {
    let mut s = HarnessSettings::default();
    s.extensions.insert(
        "core-agent".into(),
        ExtensionEntry {
            command: vec!["/usr/local/bin/my-agent".into(), "--flag".into()],
            ..Default::default()
        },
    );
    let resolved = s.resolve_extensions(builtins()).expect("resolve");
    let agent = resolved
        .iter()
        .find(|e| e.name == "core-agent")
        .expect("agent");
    assert_eq!(agent.command, "/usr/local/bin/my-agent");
    assert_eq!(agent.args, vec!["--flag"]);
    // Role is preserved from the built-in default.
    assert_eq!(agent.role.as_deref(), Some("agent"));
}

#[test]
fn resolve_extensions_adds_user_extension_keys() {
    let mut s = HarnessSettings::default();
    s.extensions.insert(
        "mything".into(),
        ExtensionEntry {
            command: vec!["/usr/local/bin/mything".into()],
            ..Default::default()
        },
    );
    let resolved = s.resolve_extensions(builtins()).expect("resolve");
    assert_eq!(resolved.len(), 4);
    let mything = resolved
        .iter()
        .find(|e| e.name == "mything")
        .expect("mything");
    assert_eq!(mything.command, "/usr/local/bin/mything");
    assert!(mything.role.is_none());
}

#[test]
fn resolve_extensions_user_extension_without_command_errors() {
    let mut s = HarnessSettings::default();
    s.extensions.insert(
        "broken".into(),
        ExtensionEntry {
            ..Default::default()
        },
    );
    let err = s.resolve_extensions(builtins()).expect_err("must err");
    match err {
        ResolveExtensionsError::EmptyCommand(name) => assert_eq!(name, "broken"),
    }
}

#[test]
fn resolve_extensions_loads_from_json5() {
    // End-to-end: a realistic harness.json5 round-trips through
    // load_json5_layered into the resolver.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.json5"),
        r#"{
                extensions: {
                    "core-shell": { enable: false },
                    "test-dummy": { enable: true },
                    "core-agent": { prefix: ["ssh", "host"] },
                    mything: { command: ["/bin/foo"] },
                },
            }"#,
    )
    .expect("write");

    let s: HarnessSettings = load_json5_layered(dir, "harness").expect("load");
    let resolved = s.resolve_extensions(builtins()).expect("resolve");
    let names: Vec<&str> = resolved.iter().map(|e| e.name.as_str()).collect();
    // core-shell dropped (disable). test-dummy enabled. core-agent
    // kept (prefix-wrapped). mything appended.
    assert_eq!(
        names,
        vec!["core-agent", "test-dummy", "core-notifications", "mything"]
    );
    let agent = &resolved[0];
    assert_eq!(agent.command, "ssh");
    assert_eq!(agent.args, vec!["host", "tau", "ext", "agent"]);
}

#[test]
fn drop_in_overrides_base() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.json5"), r#"{ greeting: true }"#).expect("write");
    std::fs::create_dir(dir.join("cli.d")).expect("mkdir");
    std::fs::write(
        dir.join("cli.d").join("01-override.json5"),
        r#"{ greeting: false }"#,
    )
    .expect("write");

    let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
    assert!(!s.greeting);
}

#[test]
fn models_load_with_providers() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("models.json5"),
        r#"{
                providers: {
                    local: {
                        baseUrl: "http://localhost:8080/v1",
                        api: "openai-completions",
                        apiKey: "test",
                        promptCacheRetention: "24h",
                        compat: {
                            supportsPromptCacheKey: true,
                            supportsPromptCacheRetention: true,
                        },
                        models: [{ id: "llama-3" }]
                    }
                }
            }"#,
    )
    .expect("write");

    let m: ModelRegistry = load_json5_layered(dir, "models").expect("load");
    assert_eq!(m.providers.len(), 1);
    let local = &m.providers["local"];
    assert_eq!(local.base_url.as_deref(), Some("http://localhost:8080/v1"));
    assert_eq!(
        local.prompt_cache_retention,
        Some(PromptCacheRetention::Extended24h)
    );
    assert!(local.compat.supports_prompt_cache_key);
    assert!(local.compat.supports_prompt_cache_retention);
    assert_eq!(local.models.len(), 1);
    assert_eq!(local.models[0].id, "llama-3");
}

#[test]
fn missing_files_return_defaults() {
    let td = TempDir::new().expect("tempdir");
    let s: CliSettings = load_json5_layered(td.path(), "cli").expect("load");
    assert!(s.greeting);
    let h: HarnessSettings = load_json5_layered(td.path(), "harness").expect("load");
    assert!(h.default_model.is_none());
    assert!(h.default_efforts.is_empty());
    let m: ModelRegistry = load_json5_layered(td.path(), "models").expect("load");
    assert!(m.providers.is_empty());
}

#[test]
fn sample_configs_deserialize() {
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
    std::fs::write(
        dir.join("models.json5"),
        include_str!("../../../../config/models.json5"),
    )
    .expect("write models");

    let _cli: CliSettings = load_json5_layered(dir, "cli").expect("cli sample should parse");
    let _harness: HarnessSettings =
        load_json5_layered(dir, "harness").expect("harness sample should parse");
    let models: ModelRegistry =
        load_json5_layered(dir, "models").expect("models sample should parse");
    assert!(
        models.providers.contains_key("local"),
        "sample models should contain 'local' provider"
    );
}
