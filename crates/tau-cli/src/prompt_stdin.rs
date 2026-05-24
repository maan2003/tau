//! One-shot stdin prompt client.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;

use tau_harness::SessionLaunchStatus;
use tau_proto::{
    ClientKind, ContentPart, ContextItem, ContextRole, Event, EventName, EventSelector, Frame,
    FrameReader, FrameWriter, Hello, Message, PROTOCOL_VERSION, PromptOriginator,
    ProviderResponseFinished, ProviderResponseUpdated, ProviderStopReason, SessionPromptTerminated,
    Subscribe, UiPromptSubmitted,
};

use crate::CliError;
use crate::daemon::{daemon_output_for_session, resolve_daemon};

/// Read a single user prompt from stdin, submit it to a daemon, print the final
/// reasoning snapshots and answer, then exit.
pub(crate) fn run_prompt_stdin(
    session_id: &str,
    attach: bool,
    session_status: SessionLaunchStatus,
    startup_role: Option<&str>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
) -> Result<(), CliError> {
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt)?;
    if prompt.is_empty() {
        return Ok(());
    }
    print_prompt_stdin_headers(session_id, startup_role);

    let daemon_output = if attach {
        None
    } else {
        Some(daemon_output_for_session(session_id)?)
    };
    let daemon = resolve_daemon(
        attach,
        session_id,
        session_status,
        daemon_output,
        startup_role,
        role_cli_overrides,
        extension_cli_overrides,
    )?;

    let (mut reader, mut writer) = connect_prompt_stdin_client(&daemon.socket_path())?;
    submit_prompt(&mut writer, session_id, prompt)?;

    let mut output = OneShotOutput::default();
    let result = read_one_shot_result(&mut reader, &mut output);

    if result.is_ok() {
        output.write_stdout()?;
    }

    disconnect_prompt_stdin_client(&mut writer);
    drop(writer);
    drop(daemon);

    result
}

type OneShotReader = FrameReader<BufReader<UnixStream>>;
type OneShotWriter = FrameWriter<BufWriter<UnixStream>>;

fn print_prompt_stdin_headers(session_id: &str, startup_role: Option<&str>) {
    eprintln!("session_id: {session_id}");
    eprintln!("role: {}", startup_role.unwrap_or("default"));
    eprintln!("agent_id: main");
}

fn connect_prompt_stdin_client(
    socket_path: &std::path::Path,
) -> io::Result<(OneShotReader, OneShotWriter)> {
    let stream = UnixStream::connect(socket_path)?;
    let read_stream = stream.try_clone()?;
    let mut writer = FrameWriter::new(BufWriter::new(stream));
    let reader = FrameReader::new(BufReader::new(read_stream));
    send_prompt_stdin_hello(&mut writer)?;
    subscribe_to_prompt_stdin_events(&mut writer)?;
    Ok((reader, writer))
}

fn send_prompt_stdin_hello(writer: &mut OneShotWriter) -> io::Result<()> {
    send_frame(
        writer,
        &Frame::Message(Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "tau-prompt-stdin".into(),
            client_kind: ClientKind::Ui,
        })),
    )
}

fn subscribe_to_prompt_stdin_events(writer: &mut OneShotWriter) -> io::Result<()> {
    send_frame(
        writer,
        &Frame::Message(Message::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Exact(EventName::PROVIDER_RESPONSE_UPDATED),
                EventSelector::Exact(EventName::PROVIDER_RESPONSE_FINISHED),
                EventSelector::Exact(EventName::SESSION_PROMPT_TERMINATED),
            ],
        })),
    )
}

fn submit_prompt(writer: &mut OneShotWriter, session_id: &str, prompt: String) -> io::Result<()> {
    send_frame(
        writer,
        &Frame::Event(Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: session_id.into(),
            text: prompt,
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: PromptOriginator::User,
            ctx_id: None,
        })),
    )
}

fn read_one_shot_result(
    reader: &mut OneShotReader,
    output: &mut OneShotOutput,
) -> Result<(), CliError> {
    loop {
        let Some(frame) = reader.read_frame().map_err(io::Error::other)? else {
            return Err(CliError::Participant("daemon disconnected".to_owned()));
        };
        if handle_prompt_stdin_frame(frame, output)? {
            return Ok(());
        }
    }
}

fn handle_prompt_stdin_frame(frame: Frame, output: &mut OneShotOutput) -> Result<bool, CliError> {
    let (_log_id, frame) = frame.peel_log();
    match frame {
        Frame::Event(Event::ProviderResponseUpdated(update)) => {
            output.capture_update(&update);
            Ok(false)
        }
        Frame::Event(Event::ProviderResponseFinished(finished)) => {
            Ok(output.capture_finished(&finished))
        }
        Frame::Event(Event::SessionPromptTerminated(terminated)) => {
            handle_prompt_terminated(&terminated)
        }
        Frame::Message(Message::Disconnect(disconnect)) => Err(CliError::Participant(
            disconnect
                .reason
                .unwrap_or_else(|| "daemon disconnected".to_owned()),
        )),
        _ => Ok(false),
    }
}

fn handle_prompt_terminated(terminated: &SessionPromptTerminated) -> Result<bool, CliError> {
    if terminated.originator.is_user() {
        return Err(CliError::Participant(format!(
            "prompt terminated: {}",
            terminated_reason(terminated)
        )));
    }
    Ok(false)
}

fn disconnect_prompt_stdin_client(writer: &mut OneShotWriter) {
    let _ = send_frame(
        writer,
        &Frame::Message(Message::Disconnect(tau_proto::Disconnect {
            reason: Some("prompt-stdin done".to_owned()),
        })),
    );
}

fn send_frame(writer: &mut OneShotWriter, frame: &Frame) -> io::Result<()> {
    writer.write_frame(frame).map_err(io::Error::other)?;
    writer.flush()
}

#[derive(Default)]
struct OneShotOutput {
    thinking_by_prompt: HashMap<String, String>,
    response_by_prompt: HashMap<String, String>,
    thinking_blocks: Vec<String>,
    final_response: Option<String>,
}

impl OneShotOutput {
    fn capture_update(&mut self, update: &ProviderResponseUpdated) {
        if !update.originator.is_user() {
            return;
        }
        let prompt_id = update.session_prompt_id.to_string();
        if let Some(thinking) = update
            .thinking
            .as_ref()
            .filter(|thinking| !thinking.is_empty())
        {
            self.thinking_by_prompt
                .insert(prompt_id.clone(), thinking.clone());
        }
        if !update.text.is_empty() {
            self.response_by_prompt
                .insert(prompt_id, update.text.clone());
        }
    }

    fn capture_finished(&mut self, finished: &ProviderResponseFinished) -> bool {
        if !finished.originator.is_user() {
            return false;
        }
        if let Some(thinking) = self
            .thinking_by_prompt
            .remove(finished.session_prompt_id.as_str())
        {
            self.thinking_blocks.push(thinking);
        }
        if finished.stop_reason.requests_tool_calls() {
            return false;
        }
        if finished.stop_reason != ProviderStopReason::Compaction {
            self.final_response =
                assistant_text_from_output_items(&finished.output_items).or_else(|| {
                    self.response_by_prompt
                        .remove(finished.session_prompt_id.as_str())
                });
        }
        true
    }

    fn write_stdout(&self) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        let mut wrote_block = false;
        for thinking in &self.thinking_blocks {
            write_text_block(&mut stdout, &mut wrote_block, thinking)?;
        }
        if let Some(response) = self.final_response.as_deref() {
            write_text_block(&mut stdout, &mut wrote_block, response)?;
        }
        if wrote_block {
            stdout.write_all(b"\n")?;
        }
        stdout.flush()
    }
}

fn write_text_block(stdout: &mut impl Write, wrote_block: &mut bool, text: &str) -> io::Result<()> {
    if *wrote_block {
        stdout.write_all(b"\n\n")?;
    }
    stdout.write_all(text.as_bytes())?;
    *wrote_block = true;
    Ok(())
}

fn assistant_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(message) if message.role == ContextRole::Assistant => Some(
                message
                    .content
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => text.as_str(),
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn terminated_reason(terminated: &SessionPromptTerminated) -> &'static str {
    match terminated.reason {
        tau_proto::SessionPromptTerminationReason::Stale => "stale",
        tau_proto::SessionPromptTerminationReason::Canceled => "canceled",
    }
}

#[cfg(test)]
mod tests {
    use tau_proto::{MessageItem, ProviderStopReason};

    use super::*;

    fn user_update(spid: &str, text: &str, thinking: Option<&str>) -> ProviderResponseUpdated {
        ProviderResponseUpdated {
            session_prompt_id: spid.into(),
            text: text.to_owned(),
            thinking: thinking.map(str::to_owned),
            originator: PromptOriginator::User,
        }
    }

    fn assistant_finished(
        spid: &str,
        text: &str,
        stop_reason: ProviderStopReason,
    ) -> ProviderResponseFinished {
        ProviderResponseFinished {
            session_prompt_id: spid.into(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: text.to_owned(),
                }],
                phase: None,
            })],
            stop_reason,
            originator: PromptOriginator::User,
            ..ProviderResponseFinished::default()
        }
    }

    /// The one-shot client ignores streaming updates for display but keeps the
    /// latest complete snapshots so finished turns can print reasoning blocks
    /// and the final answer only once the agent is done.
    #[test]
    fn one_shot_output_waits_through_tool_calls_and_keeps_final_snapshots() {
        let mut output = OneShotOutput::default();
        output.capture_update(&user_update("sp-tool", "", Some("plan v1")));
        output.capture_update(&user_update("sp-tool", "", Some("plan final")));

        assert!(!output.capture_finished(&ProviderResponseFinished {
            session_prompt_id: "sp-tool".into(),
            stop_reason: ProviderStopReason::ToolCalls,
            originator: PromptOriginator::User,
            ..ProviderResponseFinished::default()
        }));

        output.capture_update(&user_update(
            "sp-final",
            "streamed answer",
            Some("answer plan"),
        ));
        assert!(output.capture_finished(&assistant_finished(
            "sp-final",
            "final answer",
            ProviderStopReason::EndTurn,
        )));

        assert_eq!(output.thinking_blocks, vec!["plan final", "answer plan"]);
        assert_eq!(output.final_response.as_deref(), Some("final answer"));
    }

    /// Some provider paths may have accumulated streaming text but no final
    /// assistant message item; fall back to the latest full update rather than
    /// printing nothing.
    #[test]
    fn one_shot_output_falls_back_to_latest_streaming_text() {
        let mut output = OneShotOutput::default();
        output.capture_update(&user_update("sp-final", "partial", None));
        output.capture_update(&user_update("sp-final", "complete", None));

        assert!(output.capture_finished(&ProviderResponseFinished {
            session_prompt_id: "sp-final".into(),
            stop_reason: ProviderStopReason::EndTurn,
            originator: PromptOriginator::User,
            ..ProviderResponseFinished::default()
        }));

        assert_eq!(output.final_response.as_deref(), Some("complete"));
    }
}
