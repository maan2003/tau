use std::error::Error;
use std::io::{BufReader, BufWriter};

use tau_proto::{
    CborValue, ClientKind, Event, Frame, FrameReader, FrameWriter, Hello, Message,
    PROTOCOL_VERSION, Ready, ToolRegister, ToolResult, ToolSpec,
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
        match frame {
            Frame::Message(Message::Hello(_)) => {
                writer.write_frame(&Frame::Message(Message::Ready(Ready {
                    message: Some("ready".to_owned()),
                })))?;
                writer.write_frame(&Frame::Event(Event::ToolRegister(ToolRegister {
                    tool: ToolSpec {
                        name: tau_proto::ToolName::new("echo"),
                        description: Some("Echo test payloads".to_owned()),
                        parameters: None,
                        enabled_by_default: true,
                        side_effects: tau_proto::ToolSideEffects::Pure,
                    },
                })))?;
                writer.flush()?;
            }
            Frame::Event(Event::ToolInvoke(invoke)) => {
                writer.write_frame(&Frame::Event(Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    result: match invoke.arguments {
                        CborValue::Null => CborValue::Text("null".to_owned()),
                        value => value,
                    },
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                })))?;
                writer.flush()?;
            }
            Frame::Message(Message::Disconnect(_)) => return Ok(()),
            _ => {}
        }
    }
}
