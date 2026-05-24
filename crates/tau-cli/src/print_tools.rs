use std::io::Write;

use tau_harness::SessionLaunchStatus;

use crate::daemon::{daemon_output_for_session, resolve_daemon};
use crate::{CliError, mint_short_id};

pub(crate) fn run_print_tools(
    role: &str,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
) -> Result<(), CliError> {
    let session_id = mint_short_id("print-tools");
    let output = daemon_output_for_session(&session_id)?;
    let daemon = resolve_daemon(
        false,
        &session_id,
        SessionLaunchStatus::New,
        Some(output),
        Some(role),
        role_cli_overrides,
        extension_cli_overrides,
    )?;
    let tools = tau_harness::get_daemon_rendered_tool_definitions(daemon.socket_path(), role)?;

    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &tools).map_err(|error| {
        CliError::Participant(format!("failed to serialize tool definitions: {error}"))
    })?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
