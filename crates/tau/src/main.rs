fn main() -> std::process::ExitCode {
    tau_cli::main_with_args_and_components(&[
        tau_cli::Component {
            name: "ext-shell",
            runner: tau_ext_shell::run_stdio,
        },
        tau_cli::Component {
            name: "ext-test-dummy",
            runner: tau_ext_test_dummy::run_stdio,
        },
        tau_cli::Component {
            name: "ext-core-delegate",
            runner: tau_ext_core_delegate::run_stdio,
        },
        tau_cli::Component {
            name: "ext-std-notifications",
            runner: tau_ext_std_notifications::run_stdio,
        },
        tau_cli::Component {
            name: "ext-websearch-exa",
            runner: tau_ext_websearch_exa::run_stdio,
        },
    ])
}
