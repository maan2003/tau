//! Headless command submission client.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use tau_proto::{Event, HarnessInputMessage};

use crate::CliError;
use crate::ui_prompt::{DEFAULT_AGENT_ROLE, create_user_agent_prompt};

pub(crate) fn run_send(line: &str) -> Result<(), CliError> {
    let text = line.trim();
    if text.is_empty() {
        return Ok(());
    }

    let harness_path =
        find_daemon().ok_or_else(|| CliError::Participant("no running daemon".to_owned()))?;
    let mut writer = crate::ui_client::connect_ui_writer(
        &tau_harness::runtime_dir::socket_path(&harness_path),
        "tau-dev-send",
    )?;

    if let Some(event) = event_for_line(text) {
        crate::ui_client::send_message(&mut writer, &HarnessInputMessage::emit(event))?;
    }

    Ok(())
}

fn event_for_line(text: &str) -> Option<Event> {
    if text == "/quit" || text == "/detach" {
        return None;
    }
    if text == "/cancel" {
        return Some(crate::ui_events::cancel_prompt(None));
    }
    if text == "/tree" {
        return Some(crate::ui_events::tree_request(None));
    }
    if let Some(arg) = text.strip_prefix("/tree ")
        && let Ok(node_id) = arg.trim().parse::<u64>()
    {
        return Some(crate::ui_events::navigate_tree(None, node_id));
    }
    if text == "/compact" {
        return Some(crate::ui_events::compact_request(None));
    }
    if text == "/fast" || text.starts_with("/fast ") {
        return None;
    }
    if text == "/role" {
        return None;
    }
    if let Some(rest) = text.strip_prefix("/role ") {
        return role_event_for_command(rest.trim());
    }
    if let Some(model) = text.strip_prefix("/model ") {
        let model = model.trim();
        if let Ok(model) = model.parse::<tau_proto::ModelId>() {
            return Some(crate::ui_events::agent_model_select(None, model));
        }
        return None;
    }
    if let Some(command) = text.strip_prefix("!!") {
        let command = command.trim();
        if !command.is_empty() {
            return Some(crate::ui_events::shell_command(command, false, None));
        }
        return None;
    }
    if let Some(command) = text.strip_prefix('!') {
        let command = command.trim();
        if !command.is_empty() {
            return Some(crate::ui_events::shell_command(command, true, None));
        }
        return None;
    }

    Some(create_user_agent_prompt(DEFAULT_AGENT_ROLE, text, None))
}

fn role_event_for_command(rest: &str) -> Option<Event> {
    crate::ui_commands::parse_role_command(rest).ok()?
}

fn find_daemon() -> Option<PathBuf> {
    let runtime_dir = tau_harness::runtime_dir::harnesses_dir();
    for entry in std::fs::read_dir(runtime_dir).ok()?.flatten() {
        let socket = entry.path();
        if socket.extension().and_then(|ext| ext.to_str()) != Some("sock") {
            continue;
        }
        let harness_path = socket.with_extension("");
        if UnixStream::connect(&socket).is_ok() {
            return Some(harness_path);
        }
        tau_harness::runtime_dir::remove_harness_files(&harness_path);
    }
    None
}

#[cfg(test)]
mod tests;
