use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use rand::Rng;
#[cfg(test)]
use rand::{SeedableRng, rngs::StdRng};
use tau_proto::{
    ClientKind, Event, EventSelector, Frame, FrameReader, FrameWriter, Hello, Message,
    PROTOCOL_VERSION, Ready, Subscribe, ToolError, ToolRegister, ToolResult, ToolSideEffects,
    ToolSpec,
};

pub const RESTART_TEST_DUMMY_TOOL_NAME: &str = "restart_test_dummy";

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    run_with_rng(reader, writer, &mut rand::thread_rng())
}

fn run_with_rng<R, W, T>(reader: R, writer: W, rng: &mut T) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
    T: Rng + ?Sized,
{
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-test-dummy".into(),
        client_kind: ClientKind::Tool,
    })))?;
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_INVOKE)],
    })))?;
    writer.write_frame(&Frame::Event(Event::ToolRegister(ToolRegister {
        tool: ToolSpec {
            name: RESTART_TEST_DUMMY_TOOL_NAME.into(),
            description: Some(
                "Test-only tool that randomly restarts the dummy extension or returns an error"
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            })),
            side_effects: ToolSideEffects::Mutating,
        },
    })))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("test dummy tools ready".to_owned()),
    })))?;
    writer.flush()?;

    loop {
        let Some(frame) = reader.read_frame()? else {
            break;
        };
        let (_, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::ToolInvoke(invoke))
                if invoke.tool_name == RESTART_TEST_DUMMY_TOOL_NAME =>
            {
                if rng.gen_bool(0.5) {
                    writer.flush()?;
                    return Ok(());
                }
                writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    message: "restarting failed".to_owned(),
                    details: None,
                })))?;
                writer.flush()?;
            }
            Frame::Event(Event::ToolInvoke(invoke)) => {
                writer.write_frame(&Frame::Event(Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    result: tau_proto::CborValue::Map(Vec::new()),
                })))?;
                writer.flush()?;
            }
            Frame::Message(Message::Disconnect(_)) => break,
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
