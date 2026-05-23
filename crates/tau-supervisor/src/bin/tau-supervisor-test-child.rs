use std::error::Error;
use std::io::{BufReader, BufWriter};

use tau_proto::{
    Ack, CborValue, ClientKind, Event, Frame, FrameReader, FrameWriter, Hello, Message,
    PROTOCOL_VERSION, Ready, Subscribe, ToolRegister, ToolResult, ToolSpec,
};

fn main() -> Result<(), Box<dyn Error>> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = FrameReader::new(BufReader::new(stdin.lock()));
    let mut writer = FrameWriter::new(BufWriter::new(stdout.lock()));

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test-child".into(),
        client_kind: ClientKind::Tool,
    })))?;
    writer.flush()?;

    loop {
        let Some(frame) = reader.read_frame()? else {
            return Ok(());
        };
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Message(Message::Hello(_)) => {
                writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
                    selectors: vec![tau_proto::EventSelector::Exact(
                        tau_proto::EventName::TOOL_STARTED,
                    )],
                })))?;
                writer.write_frame(&Frame::Message(Message::Ready(Ready {
                    message: Some("ready".to_owned()),
                })))?;
                writer.write_frame(&Frame::Event(Event::ToolRegister(ToolRegister {
                    tool: ToolSpec {
                        name: tau_proto::ToolName::new("echo"),
                        model_visible_name: None,
                        description: Some("Echo test payloads".to_owned()),
                        tool_type: tau_proto::ToolType::Function,
                        parameters: None,
                        format: None,
                        enabled_by_default: true,
                        execution_mode: tau_proto::ToolExecutionMode::Shared,
                        background_support: None,
                    },
                    prompt_fragment: None,
                })))?;
                if let Some(id) = log_id {
                    writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                }
                writer.flush()?;
            }
            Frame::Event(Event::ToolStarted(invoke)) => {
                if invoke.tool_name != tau_proto::ToolName::new("echo") {
                    if let Some(id) = log_id {
                        writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                    }
                    writer.flush()?;
                    continue;
                }
                writer.write_frame(&Frame::Event(Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    result: match invoke.arguments {
                        CborValue::Null => CborValue::Text("null".to_owned()),
                        value => value,
                    },
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                })))?;
                if let Some(id) = log_id {
                    writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
                }
                writer.flush()?;
            }
            Frame::Message(Message::Disconnect(_)) => return Ok(()),
            _ => {}
        }
    }
}
