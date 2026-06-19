use super::*;

#[test]
fn daemon_command_sets_and_clears_harness_config_override_env() {
    let override_ = tau_config::settings::HarnessConfigCliOverride {
        key: "debug_retention_days".to_owned(),
        raw_value: "3".to_owned(),
    };
    let with_override = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            harness_config: std::slice::from_ref(&override_),
        },
    });
    assert!(with_override.get_envs().any(|(key, value)| {
        key == tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV && value.is_some()
    }));

    let without_override = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            harness_config: &[],
        },
    });
    assert!(without_override.get_envs().any(|(key, value)| {
        key == tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV && value.is_none()
    }));
}

#[test]
fn daemon_command_clears_socket_activation_env() {
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            harness_config: &[],
        },
    });

    for expected_key in [
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDS_FIRST_FD",
        "LISTEN_FDNAMES",
    ] {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == expected_key && value.is_none()),
            "expected {expected_key} to be removed from harness child environment"
        );
    }
}

#[test]
fn daemon_command_uses_initial_ui_stdio() {
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            harness_config: &[],
        },
    });

    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(args, ["component", "harness", "--initial-ui-stdio"]);
}
