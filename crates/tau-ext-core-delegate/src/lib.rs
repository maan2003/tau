//! Core delegate tool extension.
//!
//! Registers `delegate`. A tool invocation is translated into an
//! `ExtAgentQuery` side conversation. When the harness returns the
//! side agent's final text, that text is returned as the tool result.

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_proto::{
    Ack, CborValue, Event, ExtAgentQuery, Frame, FrameReader, FrameWriter, LogEventId, Message,
    ToolError, ToolInvoke, ToolResult, ToolSideEffects, ToolSpec,
};

pub const LOG_TARGET: &str = "core-delegate";
pub const TOOL_NAME: &str = "delegate";

const DELEGATE_PREFIX: &str = include_str!("../prompts/delegate_prefix.md");

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    tau_extension::Handshake::tool("tau-ext-core-delegate")
        .subscribe([
            tau_proto::EventName::TOOL_INVOKE,
            tau_proto::EventName::EXTENSION_AGENT_QUERY_RESULT,
        ])
        .register_tool(tool_spec())
        .ready_message("core-delegate ready")
        .run(&mut writer)?;
    tracing::info!(target: LOG_TARGET, tool = TOOL_NAME, "registered and ready");

    // Outstanding delegations indexed by query id. Cleanup relies on
    // the harness invariant that every `ExtAgentQuery` is answered by
    // exactly one terminal `ExtAgentQueryResult` (success or error).
    // If the harness ever drops that contract (cancellation,
    // abandoned side conversation, etc.) this map grows unbounded —
    // at that point a GC keyed off a cancellation event would be
    // needed.
    let mut pending: HashMap<String, (tau_proto::ToolCallId, tau_proto::ToolName)> = HashMap::new();
    let mut next_query_id: u64 = 0;

    loop {
        let Some(frame) = reader.read_frame()? else {
            break;
        };
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::ToolInvoke(invoke)) => {
                handle_tool_invoke(invoke, &mut pending, &mut next_query_id, &mut writer)?
            }
            Frame::Event(Event::ExtAgentQueryResult(result)) => {
                if let Some((call_id, tool_name)) = pending.remove(&result.query_id) {
                    if let Some(error) = result.error {
                        tracing::debug!(
                            target: LOG_TARGET,
                            query_id = %result.query_id,
                            error = %error,
                            "delegation failed",
                        );
                        writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
                            call_id,
                            tool_name,
                            message: error,
                            details: None,
                            display: None,
                            originator: tau_proto::PromptOriginator::User,
                        })))?;
                    } else {
                        tracing::debug!(
                            target: LOG_TARGET,
                            query_id = %result.query_id,
                            text_len = result.text.len(),
                            "delegation succeeded",
                        );
                        writer.write_frame(&Frame::Event(Event::ToolResult(ToolResult {
                            call_id,
                            tool_name,
                            result: CborValue::Text(result.text),
                            display: None,
                            originator: tau_proto::PromptOriginator::User,
                        })))?;
                    }
                    writer.flush()?;
                } else {
                    tracing::warn!(
                        target: LOG_TARGET,
                        query_id = %result.query_id,
                        "received result for unknown query_id",
                    );
                }
            }
            Frame::Message(Message::Disconnect(_)) => {
                tracing::info!(target: LOG_TARGET, "disconnect received, exiting");
                break;
            }
            // No configuration today. `Configure` (and any other
            // message variants the harness may add) is intentionally
            // ignored — the harness does not require an
            // acknowledgement. If that changes (e.g. a mandatory
            // `Configure` handshake is added) this arm must respond.
            _ => {}
        }
        if let Some(id) = log_id {
            ack_log_event(id, &mut writer)?;
        }
    }
    Ok(())
}

fn handle_tool_invoke<W: Write>(
    invoke: ToolInvoke,
    pending: &mut HashMap<String, (tau_proto::ToolCallId, tau_proto::ToolName)>,
    next_query_id: &mut u64,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    if invoke.tool_name.as_str() != TOOL_NAME {
        tracing::warn!(
            target: LOG_TARGET,
            tool = %invoke.tool_name,
            "received invoke for unknown tool",
        );
        writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            message: "unknown tool".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })))?;
        writer.flush()?;
        return Ok(());
    }

    let parsed = match parse_args(&invoke.arguments) {
        Ok(parsed) => parsed,
        Err(message) => {
            tracing::debug!(
                target: LOG_TARGET,
                error = %message,
                "rejecting delegate invocation: bad arguments",
            );
            writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message,
                details: Some(invoke.arguments),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            })))?;
            writer.flush()?;
            return Ok(());
        }
    };

    let query_id = format!("delegate-{next_query_id}");
    *next_query_id += 1;
    let call_id = invoke.call_id.clone();
    tracing::info!(
        target: LOG_TARGET,
        query_id = %query_id,
        task_name = %parsed.task_name,
        prompt_len = parsed.prompt.len(),
        "dispatching delegation",
    );
    pending.insert(query_id.clone(), (invoke.call_id, invoke.tool_name));
    writer.write_frame(&Frame::Event(Event::ExtAgentQuery(ExtAgentQuery {
        query_id,
        instruction: format!("{DELEGATE_PREFIX}{}", parsed.prompt),
        // Hand the parent call_id and the agent-supplied task name to
        // the harness so it can route sub-agent progress
        // (`DelegateProgress`) under this tool block and the CLI can
        // render `delegate [name] …`.
        tool_call_id: Some(call_id),
        task_name: Some(parsed.task_name),
    })))?;
    writer.flush()?;
    Ok(())
}

fn ack_log_event<W: Write>(
    id: LogEventId,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
    writer.flush()?;
    Ok(())
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(TOOL_NAME),
        description: Some(
            "Delegate a self-contained sub-task to a fresh sub-agent that runs with its own context and tools, and returns only its final text answer. Use it for: open-ended exploration where step count is unpredictable; large search/read sweeps whose intermediate output would otherwise clutter this conversation; parallel work — multiple `read_only: true` delegations dispatched in the same turn run concurrently. Skip it when the target is already known (use direct tools like `read`/`grep`/`shell` instead) or when the task requires synthesis you should do yourself — don't push 'based on findings, fix the bug' onto a sub-agent; investigate first, then delegate the concrete change. The sub-agent starts with a *clean* conversation: it sees ONLY your `prompt`, plus its tools and system prompt. It cannot see this conversation's prior turns, your reasoning, files you've read, or earlier tool results — and that isolation applies at every nesting depth, so a sub-agent's own delegations are equally fresh. You must therefore brief the sub-agent fully: state the goal, hand it every fact it needs (absolute file paths, exact symbols, code snippets, prior findings, constraints, format of the answer you want), and frame the sub-task as if writing to a teammate who just walked into the room. Terse command-style prompts produce shallow, generic work; missing context produces wrong answers."
                .to_owned(),
        ),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Short human-readable label for the sub-task (a few words, lowercase). Surfaced live to the user as `delegate [task_name]` while the sub-agent runs."
                },
                "prompt": {
                    "type": "string",
                    "description": "Self-contained task for the sub-agent. The sub-agent's conversation starts fresh — it has NO access to this conversation's history, your earlier tool results, or files you've read. State everything it needs: the goal, the relevant facts (absolute file paths, exact symbols, snippets you've already extracted), any constraints, what counts as 'done', and the format of the answer you want back. Treat it like briefing a teammate who just walked into the room. Terse command-style prompts produce shallow work; missing context produces wrong answers."
                },
                "read_only": {
                    "type": "boolean",
                    "description": "Set true ONLY when the sub-task is fully read-only (no file writes, no shell commands with side effects, no network mutation, no nested delegations that mutate). Read-only delegations dispatched in the same turn run concurrently with each other and with other read-only tool calls — set this whenever applicable to enable parallelism. Default: false (treated as `Mutating` — runs sequentially with other mutating work)."
                }
            },
            "required": ["task_name", "prompt"]
        })),
        enabled_by_default: true,
        // Conservative default at registration time. The harness
        // overrides this per-call when `read_only: true` is set in the
        // arguments, so two read-only delegations from the same agent
        // turn can dispatch concurrently.
        side_effects: ToolSideEffects::Mutating,
    }
}

#[cfg_attr(test, derive(Debug))]
struct DelegateArgs {
    task_name: String,
    prompt: String,
}

fn parse_args(arguments: &CborValue) -> Result<DelegateArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut prompt = None;
    let mut task_name = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "prompt" => match v {
                CborValue::Text(text) => prompt = Some(text.clone()),
                _ => return Err("`prompt` must be a string".to_owned()),
            },
            "task_name" => match v {
                CborValue::Text(text) => task_name = Some(text.clone()),
                _ => return Err("`task_name` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let prompt = prompt.ok_or_else(|| "missing string argument: prompt".to_owned())?;
    if prompt.trim().is_empty() {
        return Err("`prompt` must not be empty".to_owned());
    }
    let task_name = task_name.ok_or_else(|| "missing string argument: task_name".to_owned())?;
    if task_name.trim().is_empty() {
        return Err("`task_name` must not be empty".to_owned());
    }
    Ok(DelegateArgs { task_name, prompt })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(entries: &[(&str, CborValue)]) -> CborValue {
        CborValue::Map(
            entries
                .iter()
                .map(|(k, v)| (CborValue::Text((*k).to_owned()), v.clone()))
                .collect(),
        )
    }

    fn text(s: &str) -> CborValue {
        CborValue::Text(s.to_owned())
    }

    #[test]
    fn parses_valid_args() {
        let parsed = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
        ]))
        .expect("valid args parse");
        assert_eq!(parsed.task_name, "audit");
        assert_eq!(parsed.prompt, "do the thing");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let parsed = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("read_only", CborValue::Bool(true)),
            ("future_field", CborValue::Integer(7.into())),
        ]))
        .expect("unknown keys ignored");
        assert_eq!(parsed.task_name, "audit");
    }

    #[test]
    fn rejects_non_map_arguments() {
        let err = parse_args(&CborValue::Text("nope".to_owned())).unwrap_err();
        assert!(err.contains("arguments must be an object"), "got: {err}");
    }

    #[test]
    fn rejects_missing_prompt() {
        let err = parse_args(&args(&[("task_name", text("audit"))])).unwrap_err();
        assert!(err.contains("prompt"), "got: {err}");
    }

    #[test]
    fn rejects_missing_task_name() {
        let err = parse_args(&args(&[("prompt", text("do the thing"))])).unwrap_err();
        assert!(err.contains("task_name"), "got: {err}");
    }

    #[test]
    fn rejects_empty_prompt() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("   \n")),
        ]))
        .unwrap_err();
        assert!(err.contains("`prompt` must not be empty"), "got: {err}");
    }

    #[test]
    fn rejects_empty_task_name() {
        let err = parse_args(&args(&[
            ("task_name", text("")),
            ("prompt", text("do the thing")),
        ]))
        .unwrap_err();
        assert!(err.contains("`task_name` must not be empty"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_prompt() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", CborValue::Integer(42.into())),
        ]))
        .unwrap_err();
        assert!(err.contains("`prompt` must be a string"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_task_name() {
        let err = parse_args(&args(&[
            ("task_name", CborValue::Bool(false)),
            ("prompt", text("do the thing")),
        ]))
        .unwrap_err();
        assert!(err.contains("`task_name` must be a string"), "got: {err}");
    }
}
