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
    if text == "/fast" || text.starts_with("/fast ") {
        return None;
    }
    if text == "/role" {
        return None;
    }
    if let Some(rest) = text.strip_prefix("/role ") {
        return role_event_for_command(rest.trim());
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

fn role_event_for_command(rest: &str) -> Option<Event> {
    let mut parts = rest.split_whitespace();
    let role = parts.next()?;
    let command = parts.next();
    let value = parts.next();
    let extra = parts.next();

    match command {
        None => Some(Event::UiRoleSelect(tau_proto::UiRoleSelect {
            role: role.to_owned(),
        })),
        Some("delete") => {
            if value.is_some() {
                return None;
            }
            Some(Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
                role: role.to_owned(),
                action: tau_proto::UiRoleUpdateAction::Delete,
            }))
        }
        Some(setting) => {
            let value = value?;
            if extra.is_some() {
                return None;
            }
            let action = crate::chat::parse_role_setting_update(setting, value).ok()?;
            Some(Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
                role: role.to_owned(),
                action,
            }))
        }
    }
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

#[cfg(test)]
mod tests {
    use tau_proto::{Event, PromptOriginator, UiRoleUpdateAction};

    use super::event_for_line;

    const SESSION_ID: &str = "test-session";

    fn event(text: &str) -> Option<Event> {
        event_for_line(SESSION_ID, text)
    }

    fn prompt_text(text: &str) -> String {
        match event(text).expect("prompt event") {
            Event::UiPromptSubmitted(prompt) => {
                assert_eq!(prompt.session_id, SESSION_ID);
                assert_eq!(prompt.originator, PromptOriginator::User);
                assert_eq!(prompt.ctx_id, None);
                prompt.text
            }
            other => panic!("expected UiPromptSubmitted, got {other:?}"),
        }
    }

    /// Headless send intentionally treats interactive-only exit commands as
    /// no-ops.
    #[test]
    fn quit_and_detach_are_no_ops() {
        assert_eq!(event("/quit"), None);
        assert_eq!(event("/detach"), None);
    }

    /// `/cancel` maps to the broadcast cancel form; the harness may later
    /// retarget it.
    #[test]
    fn cancel_requests_prompt_cancellation() {
        match event("/cancel").expect("cancel event") {
            Event::UiCancelPrompt(cancel) => {
                assert_eq!(cancel.session_id, SESSION_ID);
                assert_eq!(cancel.session_prompt_id, None);
            }
            other => panic!("expected UiCancelPrompt, got {other:?}"),
        }
    }

    /// Tree commands are daemon-side operations, while malformed navigation
    /// stays a prompt.
    #[test]
    fn tree_commands_request_or_navigate_tree() {
        match event("/tree").expect("tree event") {
            Event::UiTreeRequest(req) => assert_eq!(req.session_id, SESSION_ID),
            other => panic!("expected UiTreeRequest, got {other:?}"),
        }

        match event("/tree 42").expect("navigate event") {
            Event::UiNavigateTree(req) => {
                assert_eq!(req.session_id, SESSION_ID);
                assert_eq!(req.node_id, 42);
            }
            other => panic!("expected UiNavigateTree, got {other:?}"),
        }

        assert_eq!(prompt_text("/tree nope"), "/tree nope");
    }

    /// `/compact` must reach the harness instead of being sent as prompt text.
    #[test]
    fn compact_requests_compaction() {
        match event("/compact").expect("compact event") {
            Event::UiCompactRequest(req) => assert_eq!(req.session_id, SESSION_ID),
            other => panic!("expected UiCompactRequest, got {other:?}"),
        }
    }

    /// Local configuration commands are ignored by `tau send`; they only make
    /// sense in chat UI.
    #[test]
    fn local_configuration_commands_are_ignored() {
        for command in ["/fast", "/fast on"] {
            assert_eq!(event(command), None, "{command}");
        }
    }

    /// Role selection aliases are forwarded as role-select events, with bare
    /// `/role` ignored.
    #[test]
    fn role_select_commands_pick_roles() {
        assert_eq!(event("/role"), None);

        match event("/role reviewer").expect("role select") {
            Event::UiRoleSelect(select) => assert_eq!(select.role, "reviewer"),
            other => panic!("expected UiRoleSelect, got {other:?}"),
        }

        match event("/model reviewer").expect("model role select") {
            Event::UiRoleSelect(select) => assert_eq!(select.role, "reviewer"),
            other => panic!("expected UiRoleSelect, got {other:?}"),
        }

        assert_eq!(event("/model "), None);
    }

    /// `/role <role> delete` is the headless spelling for deleting a runtime
    /// role override.
    #[test]
    fn role_delete_command_updates_roles() {
        match event("/role scratch delete").expect("role update") {
            Event::UiRoleUpdate(update) => {
                assert_eq!(update.role, "scratch");
                assert_eq!(update.action, UiRoleUpdateAction::Delete);
            }
            other => panic!("expected UiRoleUpdate, got {other:?}"),
        }
    }

    /// Shell commands produce dynamic ids but preserve command text and
    /// context-inclusion mode.
    #[test]
    fn shell_commands_record_context_mode() {
        match event("!! echo hi").expect("ui-only shell command") {
            Event::UiShellCommand(command) => {
                assert_eq!(command.session_id, SESSION_ID);
                assert!(command.command_id.as_str().starts_with("ui-sh-"));
                assert_eq!(command.command, "echo hi");
                assert!(!command.include_in_context);
            }
            other => panic!("expected UiShellCommand, got {other:?}"),
        }

        match event("! echo hi").expect("context shell command") {
            Event::UiShellCommand(command) => {
                assert_eq!(command.session_id, SESSION_ID);
                assert!(command.command_id.as_str().starts_with("ui-sh-"));
                assert_eq!(command.command, "echo hi");
                assert!(command.include_in_context);
            }
            other => panic!("expected UiShellCommand, got {other:?}"),
        }

        assert_eq!(event("!!"), None);
        assert_eq!(event("!"), None);
    }

    /// Unrecognized text is submitted unchanged as a normal user prompt.
    #[test]
    fn normal_text_submits_user_prompt() {
        assert_eq!(prompt_text("explain this diff"), "explain this diff");
    }
}
