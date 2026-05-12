use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use rand::Rng;
#[cfg(test)]
use rand::{SeedableRng, rngs::StdRng};
use tau_proto::{
    Emit, Event, EventSelector, Frame, FrameReader, FrameWriter, HarnessInfo, HarnessInfoLevel,
    InterceptAction, InterceptReply, InterceptionPriority, Message, ToolError, ToolSideEffects,
    ToolSpec, UiPromptSubmitted,
};

pub const RESTART_TEST_DUMMY_TOOL_NAME: &str = "restart_test_dummy";

/// Returns a copy of `text` with every case-insensitive "tao" word
/// rewritten to "tau", preserving the original casing letter-by-letter
/// (so `Tao` → `Tau`, `TAO` → `TAU`, `tAo` → `tAu`). Returns `None` if
/// no replacement happened so the caller can short-circuit and reply
/// with `Pass(None)` rather than re-publish an identical event.
///
/// "tao" is matched as a whole word, not as a free-floating
/// substring — the `tao` inside `taoism` is left alone.
fn correct_tao_to_tau(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut changed = false;
    while i < bytes.len() {
        let is_match = i + 3 <= bytes.len()
            && bytes[i].eq_ignore_ascii_case(&b't')
            && bytes[i + 1].eq_ignore_ascii_case(&b'a')
            && bytes[i + 2].eq_ignore_ascii_case(&b'o')
            && !preceded_by_letter(bytes, i)
            && !followed_by_letter(bytes, i + 3);
        if is_match {
            out.push(bytes[i] as char);
            out.push(bytes[i + 1] as char);
            // Replace 'o'/'O' → 'u'/'U' matching the original case.
            out.push(if bytes[i + 2].is_ascii_uppercase() {
                'U'
            } else {
                'u'
            });
            i += 3;
            changed = true;
        } else {
            // Cheap path for ASCII; fall back to a char step at the
            // current byte boundary to stay UTF-8-safe.
            let ch = text[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    changed.then_some(out)
}

fn preceded_by_letter(bytes: &[u8], i: usize) -> bool {
    i > 0 && bytes[i - 1].is_ascii_alphabetic()
}

fn followed_by_letter(bytes: &[u8], i: usize) -> bool {
    bytes.get(i).is_some_and(|b| b.is_ascii_alphabetic())
}

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

    tau_extension::Handshake::tool("tau-ext-test-dummy")
        .intercept(
            EventSelector::Exact(tau_proto::EventName::UI_PROMPT_SUBMITTED),
            InterceptionPriority::new(0),
        )
        .register_tool(ToolSpec {
            name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
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
        })
        .ready_message("test dummy tools ready")
        .run(&mut writer)?;

    loop {
        let Some(frame) = reader.read_frame()? else {
            break;
        };
        let (_, inner) = frame.peel_log();
        match inner {
            Frame::Message(Message::InterceptRequest(req)) => {
                let mutated = match req.event.as_ref() {
                    Event::UiPromptSubmitted(prompt) => {
                        correct_tao_to_tau(&prompt.text).map(|fixed| {
                            Event::UiPromptSubmitted(UiPromptSubmitted {
                                text: fixed,
                                ..prompt.clone()
                            })
                        })
                    }
                    _ => None,
                };
                if mutated.is_some() {
                    writer.write_frame(&Frame::Message(Message::Emit(Emit {
                        event: Box::new(Event::HarnessInfo(HarnessInfo {
                            message: "did you mean \"Tau\"? — corrected for you".to_owned(),
                            level: HarnessInfoLevel::Normal,
                        })),
                        transient: true,
                    })))?;
                }
                let action = match mutated {
                    Some(event) => InterceptAction::Pass(Some(Box::new(event))),
                    None => InterceptAction::Pass(None),
                };
                writer.write_frame(&Frame::Message(Message::InterceptReply(InterceptReply {
                    action,
                })))?;
                writer.flush()?;
            }
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
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
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
