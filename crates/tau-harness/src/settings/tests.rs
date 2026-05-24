use tau_config::settings::{ExtensionEntry, HarnessSettings, load_harness_settings_in};
use tempfile::TempDir;

use super::*;

fn builtin(
    name: &str,
    suffix_arg: &str,
    role: &str,
    enable: bool,
    config: serde_json::Value,
) -> BuiltinExtension {
    BuiltinExtension {
        name: name.to_owned(),
        prefix: Vec::new(),
        command: vec!["tau".into()],
        suffix: vec!["ext".into(), suffix_arg.into()],
        role: Some(role.into()),
        enable,
        config,
        secrets: BTreeMap::new(),
    }
}

fn builtins() -> Vec<BuiltinExtension> {
    vec![
        builtin(
            "provider-builtin",
            "ext-provider-builtin",
            "provider",
            true,
            serde_json::json!({}),
        ),
        builtin(
            "core-shell",
            "ext-shell",
            "tool",
            true,
            serde_json::json!({}),
        ),
        builtin(
            "test-dummy",
            "ext-test-dummy",
            "tool",
            false,
            serde_json::json!({}),
        ),
        builtin(
            "std-notifications",
            "ext-std-notifications",
            "tool",
            true,
            serde_json::json!({ "idle_seconds": 60, "idle_agent_summary": false }),
        ),
        builtin(
            "std-websearch",
            "ext-websearch",
            "tool",
            true,
            serde_json::json!({}),
        ),
        builtin(
            "std-email",
            "ext-email",
            "tool",
            false,
            serde_json::json!({}),
        ),
    ]
}

#[test]
fn resolve_extensions_returns_builtins_when_user_config_empty() {
    let s = HarnessSettings::built_in();
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert_eq!(resolved.len(), 4);
    assert_eq!(resolved[0].name, "provider-builtin");
    assert_eq!(resolved[0].command, "tau");
    assert_eq!(resolved[0].args, vec!["ext", "ext-provider-builtin"]);
    assert_eq!(resolved[0].role.as_deref(), Some("provider"));
    assert_eq!(resolved[1].name, "core-shell");
    assert_eq!(resolved[2].name, "std-notifications");
    assert_eq!(resolved[3].name, "std-websearch");
}

#[test]
fn resolve_extensions_builtin_can_start_disabled() {
    let s = HarnessSettings::built_in();
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
    assert!(resolved.iter().all(|e| e.name != "std-email"));
}

#[test]
fn resolve_extensions_enables_disabled_std_email_builtin() {
    // The standard email extension ships disabled. A user opt-in should keep
    // the built-in tau subcommand suffix and place the entry at its built-in
    // order position.
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "std-email".into(),
        ExtensionEntry {
            enable: Some(true),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let email = resolved
        .iter()
        .find(|e| e.name == "std-email")
        .expect("std-email enabled");
    assert_eq!(email.command, "tau");
    assert_eq!(email.args, vec!["ext", "ext-email"]);
    assert_eq!(email.role.as_deref(), Some("tool"));
}

#[test]
fn resolve_extensions_cli_overrides_apply_after_user_config() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "core-shell".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );
    s.extensions.insert(
        "test-dummy".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions_with_cli_overrides(
        &s,
        builtins(),
        &[
            tau_config::settings::ExtensionCliOverride::EnableAll,
            tau_config::settings::ExtensionCliOverride::Disable("std-websearch".to_owned()),
            tau_config::settings::ExtensionCliOverride::Enable("test-dummy".to_owned()),
        ],
    )
    .expect("resolve");
    let names = resolved
        .iter()
        .map(|extension| extension.name.as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"core-shell"));
    assert!(names.contains(&"test-dummy"));
    assert!(!names.contains(&"std-websearch"));
}

#[test]
fn resolve_extensions_cli_can_enable_disabled_user_extension() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "future-extension".into(),
        ExtensionEntry {
            command: Some(vec!["future-extension".to_owned()]),
            enable: Some(false),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions_with_cli_overrides(
        &s,
        builtins(),
        &[tau_config::settings::ExtensionCliOverride::Enable(
            "future-extension".to_owned(),
        )],
    )
    .expect("resolve");

    assert!(
        resolved
            .iter()
            .any(|extension| extension.name == "future-extension")
    );
}

#[test]
fn resolve_extensions_cli_enable_unknown_extension_is_ignored() {
    let s = HarnessSettings::built_in();
    let resolved = resolve_extensions_with_cli_overrides(
        &s,
        builtins(),
        &[tau_config::settings::ExtensionCliOverride::Enable(
            "missing".to_owned(),
        )],
    )
    .expect("resolve");

    assert!(resolved.iter().all(|extension| extension.name != "missing"));
}

#[test]
fn resolve_extensions_disable_drops_entry() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "core-shell".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert_eq!(resolved.len(), 3);
    assert_eq!(resolved[0].name, "provider-builtin");
    assert_eq!(resolved[1].name, "std-notifications");
    assert_eq!(resolved[2].name, "std-websearch");
}

#[test]
fn resolve_extensions_prefix_wraps_builtin_command() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "provider-builtin".into(),
        ExtensionEntry {
            prefix: Some(vec!["ssh".into(), "user@host".into()]),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let provider = resolved
        .iter()
        .find(|e| e.name == "provider-builtin")
        .expect("provider");
    // argv[0] is the wrapper; original command moves into args.
    assert_eq!(provider.command, "ssh");
    assert_eq!(
        provider.args,
        vec!["user@host", "tau", "ext", "ext-provider-builtin"]
    );
}

#[test]
fn resolve_extensions_user_command_replaces_builtin_command() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "provider-builtin".into(),
        ExtensionEntry {
            command: Some(vec!["/usr/local/bin/my-provider".into(), "--flag".into()]),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let provider = resolved
        .iter()
        .find(|e| e.name == "provider-builtin")
        .expect("provider");
    assert_eq!(provider.command, "/usr/local/bin/my-provider");
    assert_eq!(provider.args, vec!["--flag"]);
    // Role is preserved from the built-in default.
    assert_eq!(provider.role.as_deref(), Some("provider"));
}

#[test]
fn resolve_extensions_adds_user_extension_keys() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "mything".into(),
        ExtensionEntry {
            command: Some(vec!["/usr/local/bin/mything".into()]),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert_eq!(resolved.len(), 5);
    let mything = resolved
        .iter()
        .find(|e| e.name == "mything")
        .expect("mything");
    assert_eq!(mything.command, "/usr/local/bin/mything");
    assert!(mything.role.is_none());
}

#[test]
fn resolve_extensions_empty_entry_does_not_re_enable_disabled_builtin() {
    // `extensions: { "test-dummy": {} }` MUST leave the
    // builtin's `enable: false` intact — absent fields mean "no
    // override", not "use the wire default". See review item #4.
    let mut s = HarnessSettings::built_in();
    s.extensions
        .insert("test-dummy".into(), ExtensionEntry::default());
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
    assert!(resolved.iter().all(|e| e.name != "std-email"));
}

#[test]
fn resolve_extensions_user_extension_without_command_errors() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "broken".into(),
        ExtensionEntry {
            ..Default::default()
        },
    );
    let err = resolve_extensions(&s, builtins()).expect_err("must err");
    match err {
        ResolveExtensionsError::EmptyCommand(name) => assert_eq!(name, "broken"),
    }
}

#[test]
fn resolve_extensions_disabled_user_extension_without_command_is_inert() {
    // A disabled custom extension should be a harmless config placeholder. In
    // particular, it must not require a command just to be dropped from the
    // resolved extension set.
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "future-extension".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&s, builtins()).expect("disabled entry is dropped");

    assert!(resolved.iter().all(|e| e.name != "future-extension"));
}

#[test]
fn resolve_extensions_loads_from_yaml() {
    // End-to-end: a realistic harness.yaml round-trips through the
    // tau-config loader into the tau-harness resolver.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
                extensions: {
                    "core-shell": { enable: false },
                    "test-dummy": { enable: true },
                    "provider-builtin": { prefix: ["ssh", "host"] },
                    mything: { command: ["/bin/foo"] },
                },
            }"#,
    )
    .expect("write");

    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(dir.to_owned()),
        state_dir: None,
    };
    let s = load_harness_settings_in(&dirs).expect("load");
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let names: Vec<&str> = resolved.iter().map(|e| e.name.as_str()).collect();
    // core-shell dropped (disable). test-dummy enabled. provider-builtin
    // kept (prefix-wrapped). mything appended.
    assert_eq!(
        names,
        vec![
            "provider-builtin",
            "test-dummy",
            "std-notifications",
            "std-websearch",
            "mything"
        ]
    );
    let provider = &resolved[0];
    assert_eq!(provider.command, "ssh");
    assert_eq!(
        provider.args,
        vec!["host", "tau", "ext", "ext-provider-builtin"]
    );
}

/// Force a parse of `config/built-in.extensions.yaml` so a
/// malformed file blows up here rather than at user startup.
#[test]
fn built_in_extensions_yaml_parses() {
    let _ = built_in_extension_defs();
}

#[test]
fn built_in_extensions_json5_contains_disabled_std_email() {
    // Guard the real embedded JSON5, not the local test fixture, so the
    // disabled-by-default email extension keeps the documented tau ext suffix
    // and tool role when future built-ins are edited.
    let email = built_in_extension_defs()
        .iter()
        .find(|def| def.name == "std-email")
        .expect("std-email built-in");
    assert!(!email.enable);
    assert_eq!(
        email.suffix.as_deref(),
        Some(["ext".to_owned(), "ext-email".to_owned()].as_slice())
    );
    assert_eq!(email.role.as_deref(), Some("tool"));
}

#[test]
fn resolve_extensions_carries_and_merges_secret_declarations() {
    let mut builtins = builtins();
    builtins[0].secrets.insert(
        "builtin_secret".into(),
        tau_config::settings::ExtensionSecretEntry::default(),
    );
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "provider-builtin".into(),
        ExtensionEntry {
            secrets: Some(BTreeMap::from([(
                "user_secret".into(),
                tau_config::settings::ExtensionSecretEntry { optional: true },
            )])),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&s, builtins).expect("resolve");
    let provider = resolved
        .iter()
        .find(|e| e.name == "provider-builtin")
        .expect("provider");
    assert!(!provider.secrets["builtin_secret"].optional);
    assert!(provider.secrets["user_secret"].optional);
}

#[test]
fn resolve_extensions_drops_disabled_entries_with_secret_declarations() {
    let mut builtins = builtins();
    builtins[2].secrets.insert(
        "required_secret".into(),
        tau_config::settings::ExtensionSecretEntry::default(),
    );
    let s = HarnessSettings::built_in();

    let resolved = resolve_extensions(&s, builtins).expect("resolve");

    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
}
