use std::io::Write;

use tau_harness::SessionLaunchStatus;

use crate::daemon::{daemon_output_for_session, resolve_daemon};
use crate::{CliError, mint_short_id};

pub(crate) fn run_print_prompt(role: &str) -> Result<(), CliError> {
    let session_id = mint_short_id("print-prompt");
    let output = daemon_output_for_session(&session_id)?;
    let daemon = resolve_daemon(false, &session_id, SessionLaunchStatus::New, Some(output))?;
    let prompt = tau_harness::get_daemon_rendered_system_prompt(daemon.socket_path(), role)?;

    let mut stdout = std::io::stdout().lock();
    stdout.write_all(prompt.as_bytes())?;
    stdout.flush()?;
    Ok(())
}
