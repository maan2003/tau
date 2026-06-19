use std::collections::BTreeMap;

use super::*;

fn test_extension_config(cwd: Option<PathBuf>) -> ExtensionConfig {
    ExtensionConfig {
        name: "test-extension".to_owned(),
        command: "tau-test-extension".to_owned(),
        args: vec!["--stdio".to_owned()],
        role: None,
        require: true,
        cwd,
        config: serde_json::json!({}),
        secrets: BTreeMap::new(),
    }
}

#[test]
fn extension_stderr_log_path_rejects_unsafe_extension_names() {
    // Extension names originate in user-authored harness config. The
    // stderr log path is constructed before the Configure handshake, so it
    // must reject traversal and absolute-path names on its own.
    let state_dir = Path::new("/tmp/tau-state");
    assert_eq!(
        extension_stderr_log_path(state_dir, "std-email").expect("safe extension name"),
        state_dir.join("logs").join("std-email.log")
    );

    for name in ["", "../x", "a/b", "/tmp/x", ".", ".."] {
        assert!(
            extension_stderr_log_path(state_dir, name).is_err(),
            "{name:?} must be rejected before building the log path"
        );
    }
}

#[test]
fn supervised_command_uses_configured_cwd() {
    // The cwd field is resolved from harness.yaml and must affect the OS
    // child process, not the LifecycleConfigure payload sent after spawn.
    let cwd = PathBuf::from("/tmp/tau-extension-cwd");
    let config = test_extension_config(Some(cwd.clone()));

    let command = supervised_command(&config, false);

    assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
}
