//! Headless command submission client.

use std::io::{self, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use tau_proto::{
    ClientKind, Event, Frame, FrameWriter, Hello, Message, PROTOCOL_VERSION, UiPromptSubmitted,
};

use crate::CliError;

pub(crate) fn run_send(session_id: &str, line: &str) -> Result<(), CliError> {
    let text = line.trim();
    if text.is_empty() {
        return Ok(());
    }

    let daemon_dir = find_daemon_for_session(session_id).ok_or_else(|| {
        CliError::Participant(format!("no running daemon for session `{session_id}`"))
    })?;
    let stream = UnixStream::connect(tau_harness::runtime_dir::socket_path(&daemon_dir))?;
    let mut writer = FrameWriter::new(BufWriter::new(stream));

    send_frame(
        &mut writer,
        &Frame::Message(Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "tau-dev-send".into(),
            client_kind: ClientKind::Ui,
        })),
    )?;

    if let Some(event) = event_for_line(session_id, text) {
        send_frame(&mut writer, &Frame::Event(event))?;
    }

    Ok(())
}

fn send_frame(writer: &mut FrameWriter<BufWriter<UnixStream>>, frame: &Frame) -> io::Result<()> {
    writer.write_frame(frame).map_err(io::Error::other)?;
    writer.flush()
}

fn event_for_line(session_id: &str, text: &str) -> Option<Event> {
    if text == "/quit" || text == "/detach" {
        return None;
    }
    if text == "/cancel" {
        return Some(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: session_id.into(),
            session_prompt_id: None,
        }));
    }
    if text == "/tree" {
        return Some(Event::UiTreeRequest(tau_proto::UiTreeRequest {
            session_id: session_id.into(),
        }));
    }
    if let Some(arg) = text.strip_prefix("/tree ")
        && let Ok(node_id) = arg.trim().parse::<u64>()
    {
        return Some(Event::UiNavigateTree(tau_proto::UiNavigateTree {
            session_id: session_id.into(),
            node_id,
        }));
    }
    if text == "/compact" {
        return Some(Event::UiCompactRequest(tau_proto::UiCompactRequest {
            session_id: session_id.into(),
        }));
    }
    if let Some(arg) = text.strip_prefix("/effort ")
        && let Ok(level) = arg.trim().parse::<tau_proto::Effort>()
    {
        return Some(Event::UiSetEffort(tau_proto::UiSetEffort { level }));
    }
    if let Some(arg) = text.strip_prefix("/verbosity ")
        && let Ok(level) = arg.trim().parse::<tau_proto::Verbosity>()
    {
        return Some(Event::UiSetVerbosity(tau_proto::UiSetVerbosity { level }));
    }
    if let Some(arg) = text.strip_prefix("/thinking-summary ")
        && let Ok(level) = arg.trim().parse::<tau_proto::ThinkingSummary>()
    {
        return Some(Event::UiSetThinkingSummary(
            tau_proto::UiSetThinkingSummary { level },
        ));
    }
    if let Some(role) = text.strip_prefix("/model ") {
        let role = role.trim();
        if !role.is_empty() {
            return Some(Event::UiRoleSelect(tau_proto::UiRoleSelect {
                role: role.to_owned(),
            }));
        }
        return None;
    }
    if let Some(command) = text.strip_prefix("!!") {
        let command = command.trim();
        if !command.is_empty() {
            return Some(shell_command(session_id, command, false));
        }
        return None;
    }
    if let Some(command) = text.strip_prefix('!') {
        let command = command.trim();
        if !command.is_empty() {
            return Some(shell_command(session_id, command, true));
        }
        return None;
    }

    Some(Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: session_id.into(),
        text: text.to_owned(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }))
}

fn shell_command(session_id: &str, command: &str, include_in_context: bool) -> Event {
    use std::time::{SystemTime, UNIX_EPOCH};

    let command_id = format!(
        "ui-sh-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: session_id.into(),
        command_id: command_id.into(),
        command: command.to_owned(),
        include_in_context,
    })
}

fn find_daemon_for_session(session_id: &str) -> Option<PathBuf> {
    let runtime_dir = tau_harness::runtime_dir::root_runtime_dir();
    for entry in std::fs::read_dir(runtime_dir).ok()?.flatten() {
        let daemon_dir = entry.path();
        if tau_harness::runtime_dir::read_session_id(&daemon_dir).as_deref() != Some(session_id) {
            continue;
        }
        if UnixStream::connect(tau_harness::runtime_dir::socket_path(&daemon_dir)).is_ok() {
            return Some(daemon_dir);
        }
        let _ = std::fs::remove_dir_all(daemon_dir);
    }
    None
}
