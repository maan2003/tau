//! Core delegate tool extension.
//!
//! Registers `delegate`. A tool invocation is translated into an
//! `ExtAgentQuery` side conversation. When the harness returns the
//! side agent's final text, that text is returned as the tool result.

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_proto::{
    Ack, BackgroundSupport, CborValue, Event, ExtAgentQuery, ExtSessionContextPublish,
    ExtensionContextReady, Frame, FrameReader, FrameWriter, HarnessRolesAvailable, LogEventId,
    Message, SessionContextKey, SessionContextValue, SessionStarted, ToolDisplayStats, ToolError,
    ToolExecutionMode, ToolInvoke, ToolResult, ToolSpec,
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
            tau_proto::EventName::SESSION_STARTED,
            tau_proto::EventName::HARNESS_ROLES_AVAILABLE,
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
    let mut current_session_id: Option<tau_proto::SessionId> = None;
    let mut latest_roles: Vec<serde_json::Value> = Vec::new();

    while let Some(frame) = reader.read_frame()? {
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
                            tool_type: tau_proto::ToolType::Function,
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
                            tool_type: tau_proto::ToolType::Function,
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
            Frame::Event(Event::SessionStarted(SessionStarted { session_id, .. })) => {
                current_session_id = Some(session_id.clone());
                publish_delegate_roles_context_and_ready(
                    &mut writer,
                    session_id.clone(),
                    &latest_roles,
                )?;
            }
            Frame::Event(Event::HarnessRolesAvailable(HarnessRolesAvailable { roles })) => {
                latest_roles = roles
                    .into_iter()
                    .map(|role| {
                        serde_json::json!({
                            "name": role.name,
                            "description": role.role_description.unwrap_or(role.description),
                        })
                    })
                    .collect();
                latest_roles.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                if let Some(session_id) = current_session_id.clone() {
                    publish_delegate_roles_context(&mut writer, session_id, &latest_roles)?;
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
            tool_type: tau_proto::ToolType::Function,
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
                tool_type: tau_proto::ToolType::Function,
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
        execution_mode = ?parsed.execution_mode,
        prompt_len = parsed.prompt.len(),
        role = ?parsed.role,
        "dispatching delegation",
    );
    pending.insert(query_id.clone(), (invoke.call_id, invoke.tool_name));
    writer.write_frame(&Frame::Event(Event::ExtAgentQuery(ExtAgentQuery {
        query_id,
        instruction: format!("{DELEGATE_PREFIX}{}", parsed.prompt),
        role: parsed.role,
        execution_mode: parsed.execution_mode,
        input_stats: ToolDisplayStats::for_text(&parsed.prompt),
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

fn publish_delegate_roles_context<W: Write>(
    writer: &mut FrameWriter<BufWriter<W>>,
    session_id: tau_proto::SessionId,
    roles: &[serde_json::Value],
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Event(Event::ExtSessionContextPublish(
        ExtSessionContextPublish {
            session_id,
            key: SessionContextKey::new("delegate_roles"),
            value: SessionContextValue(serde_json::Value::Array(roles.to_vec())),
        },
    )))?;
    writer.flush()?;
    Ok(())
}

fn publish_delegate_roles_context_and_ready<W: Write>(
    writer: &mut FrameWriter<BufWriter<W>>,
    session_id: tau_proto::SessionId,
    roles: &[serde_json::Value],
) -> Result<(), Box<dyn Error>> {
    // The harness waits for every supervised tool extension subscribed to
    // `session.started` to report that its per-session context is complete.
    // Delegate subscribes only to publish the prompt-visible role list, so the
    // readiness signal belongs immediately after that publish.
    publish_delegate_roles_context(writer, session_id.clone(), roles)?;
    writer.write_frame(&Frame::Event(Event::ExtensionContextReady(
        ExtensionContextReady { session_id },
    )))?;
    writer.flush()?;
    Ok(())
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Delegate a self-contained sub-task to a fresh sub-agent that runs with its own context and tools, and returns only its final text answer. Use it for: open-ended exploration where step count is unpredictable; large search/read sweeps whose intermediate output would otherwise clutter this conversation; parallel work — multiple delegations with `execution_mode: \"shared\"` can overlap globally. Use `execution_mode: \"exclusive\"` when the sub-agent needs to run alone: it waits for all other sub-agent delegations and blocks later independent ones until it finishes. Skip it when the target is already known (use direct tools like `read`/`grep`/`shell` instead) or when the task requires synthesis you should do yourself — don't push 'based on findings, fix the bug' onto a sub-agent; investigate first, then delegate the concrete change. The sub-agent starts with a *clean* conversation: it sees ONLY your `prompt`, plus its tools and system prompt. It cannot see this conversation's prior turns, your reasoning, files you've read, or earlier tool results — and that isolation applies at every nesting depth, so a sub-agent's own delegations are equally fresh. You must therefore brief the sub-agent fully: state the goal, hand it every fact it needs (absolute file paths, exact symbols, code snippets, prior findings, constraints, format of the answer you want), and frame the sub-task as if writing to a teammate who just walked into the room. Terse command-style prompts produce shallow, generic work; missing context produces wrong answers."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
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
                "execution_mode": {
                    "type": "string",
                    "enum": ["shared", "exclusive"],
                    "description": "Use `shared` when the sub-task can safely overlap globally with other shared sub-agent delegations. Use `exclusive` when it must run alone: it waits for all other sub-agent delegations and blocks later independent ones. Default: `shared`."
                },
                "role": {
                    "type": "string",
                    "description": "Optional sub-agent role to use. When omitted, Tau defaults delegate calls to `engineer` if that role is available and enabled."
                }
            },
            "required": ["task_name", "prompt"]
        })),
        format: None,
        enabled_by_default: true,
        // The delegate tool itself can dispatch alongside other shared
        // tools in the parent conversation; the `ExtAgentQuery` global
        // scheduler below enforces per-delegation exclusivity.
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Instant),
    }
}

#[cfg_attr(test, derive(Debug))]
struct DelegateArgs {
    task_name: String,
    prompt: String,
    execution_mode: ToolExecutionMode,
    role: Option<String>,
}

fn parse_args(arguments: &CborValue) -> Result<DelegateArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut prompt = None;
    let mut task_name = None;
    let mut execution_mode = None;
    let mut role = None;
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
            "role" => match v {
                CborValue::Text(text) => role = Some(text.clone()),
                _ => return Err("`role` must be a string".to_owned()),
            },
            "execution_mode" => match v {
                CborValue::Text(text) if text == "shared" => {
                    execution_mode = Some(ToolExecutionMode::Shared)
                }
                CborValue::Text(text) if text == "exclusive" => {
                    execution_mode = Some(ToolExecutionMode::Exclusive)
                }
                CborValue::Text(_) => {
                    return Err("`execution_mode` must be `shared` or `exclusive`".to_owned());
                }
                _ => return Err("`execution_mode` must be a string".to_owned()),
            },
            // Compatibility with the pre-`execution_mode` schema. Only true was
            // meaningful: it requested overlap for safe read-only work, which is
            // now represented as Shared. Explicit `execution_mode` wins because
            // omitted mode also defaults to Shared below.
            "read_only" => match v {
                CborValue::Bool(_) => {}
                _ => return Err("`read_only` must be a boolean".to_owned()),
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
    let role = role.filter(|role| !role.trim().is_empty());
    let execution_mode = execution_mode.unwrap_or(ToolExecutionMode::Shared);
    Ok(DelegateArgs {
        task_name,
        prompt,
        execution_mode,
        role,
    })
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
        assert_eq!(parsed.execution_mode, ToolExecutionMode::Shared);
        assert_eq!(parsed.role, None);
    }

    /// Delegate role is optional metadata passed through to the harness; empty
    /// strings are treated as omitted so older agents that hallucinate blank
    /// fields do not change behavior.
    #[test]
    fn parses_optional_role() {
        let parsed = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("role", text("rush")),
        ]))
        .expect("role args parse");
        assert_eq!(parsed.role.as_deref(), Some("rush"));

        let omitted = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("role", text("   ")),
        ]))
        .expect("blank role args parse");
        assert_eq!(omitted.role, None);
    }

    /// Regression coverage for global sub-agent scheduling intent: omitted
    /// execution_mode is shared, explicit exclusive is preserved, and legacy
    /// read_only:true remains accepted as shared for older agents.
    #[test]
    fn parses_execution_mode_and_legacy_read_only() {
        let exclusive = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("execution_mode", text("exclusive")),
            ("read_only", CborValue::Bool(true)),
        ]))
        .expect("explicit execution_mode parses");
        assert_eq!(exclusive.execution_mode, ToolExecutionMode::Exclusive);

        let legacy = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("read_only", CborValue::Bool(true)),
        ]))
        .expect("legacy read_only parses");
        assert_eq!(legacy.execution_mode, ToolExecutionMode::Shared);
    }

    /// Regression coverage for the agent-visible terminology: delegate should
    /// advertise shared/exclusive execution modes plus optional role selection,
    /// and keep the legacy alias out of the schema/description.
    #[test]
    fn tool_schema_advertises_execution_mode_and_role() {
        let spec = tool_spec();
        let description = spec.description.expect("description");
        assert!(description.contains("execution_mode"));
        assert!(description.contains("shared"));
        assert!(!description.contains("read_only"));
        assert!(!description.contains("read-only"));
        assert!(!description.contains("Mutating"));

        let parameters = spec.parameters.expect("parameters");
        let properties = parameters
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("object properties");
        assert!(properties.contains_key("execution_mode"));
        assert!(properties.contains_key("role"));
        assert!(!properties.contains_key("read_only"));
        assert_eq!(spec.execution_mode, ToolExecutionMode::Shared);
        assert_eq!(spec.background_support, Some(BackgroundSupport::Instant));
    }

    /// Delegate progress should show the size of the user-provided prompt,
    /// not the private prefix inserted before the sub-agent sees it.
    #[test]
    fn tool_invoke_reports_input_stats_for_user_prompt_only() {
        let prompt = "first\nsecond";
        let mut pending = HashMap::new();
        let mut next_query_id = 0;
        let mut output = Vec::new();
        {
            let mut writer = FrameWriter::new(BufWriter::new(&mut output));
            handle_tool_invoke(
                ToolInvoke {
                    call_id: "call-1".into(),
                    tool_name: tau_proto::ToolName::new(TOOL_NAME),
                    arguments: args(&[("task_name", text("audit")), ("prompt", text(prompt))]),
                    originator: tau_proto::PromptOriginator::User,
                },
                &mut pending,
                &mut next_query_id,
                &mut writer,
            )
            .expect("delegate invocation handled");
        }

        let mut reader = FrameReader::new(BufReader::new(output.as_slice()));
        let frame = reader
            .read_frame()
            .expect("read query frame")
            .expect("query frame present");
        let Frame::Event(Event::ExtAgentQuery(query)) = frame else {
            panic!("expected ExtAgentQuery, got {frame:?}");
        };
        assert!(query.instruction.starts_with(DELEGATE_PREFIX));
        assert!(query.instruction.ends_with(prompt));
        assert_eq!(query.input_stats, ToolDisplayStats::for_text(prompt));
        assert_ne!(
            query.input_stats,
            ToolDisplayStats::for_text(&query.instruction)
        );
    }

    /// Regression coverage for the 9c3088c "don't special case foreman"
    /// change: subscribing to `session.started` makes the harness wait for this
    /// extension during session init, so delegate must pair its context publish
    /// with `extension.context_ready`.
    #[test]
    fn session_started_context_publish_is_followed_by_context_ready() {
        let mut output = Vec::new();
        {
            let mut writer = FrameWriter::new(BufWriter::new(&mut output));
            publish_delegate_roles_context_and_ready(
                &mut writer,
                "s1".into(),
                &[serde_json::json!({
                    "name": "deep",
                    "description": "Deep research",
                })],
            )
            .expect("publish context and ready");
        }

        let mut reader = FrameReader::new(BufReader::new(output.as_slice()));
        let first = reader
            .read_frame()
            .expect("read context frame")
            .expect("context frame present");
        assert!(matches!(
            first,
            Frame::Event(Event::ExtSessionContextPublish(publish))
                if publish.session_id.as_str() == "s1"
                    && publish.key.as_str() == "delegate_roles"
        ));

        let second = reader
            .read_frame()
            .expect("read ready frame")
            .expect("ready frame present");
        assert!(matches!(
            second,
            Frame::Event(Event::ExtensionContextReady(ready))
                if ready.session_id.as_str() == "s1"
        ));
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
        let err = parse_args(&CborValue::Text("nope".to_owned()))
            .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("arguments must be an object"), "got: {err}");
    }

    #[test]
    fn rejects_missing_prompt() {
        let err = parse_args(&args(&[("task_name", text("audit"))]))
            .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("prompt"), "got: {err}");
    }

    #[test]
    fn rejects_missing_task_name() {
        let err = parse_args(&args(&[("prompt", text("do the thing"))]))
            .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("task_name"), "got: {err}");
    }

    #[test]
    fn rejects_empty_prompt() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("   \n")),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`prompt` must not be empty"), "got: {err}");
    }

    #[test]
    fn rejects_empty_task_name() {
        let err = parse_args(&args(&[
            ("task_name", text("")),
            ("prompt", text("do the thing")),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`task_name` must not be empty"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_prompt() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", CborValue::Integer(42.into())),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`prompt` must be a string"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_task_name() {
        let err = parse_args(&args(&[
            ("task_name", CborValue::Bool(false)),
            ("prompt", text("do the thing")),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`task_name` must be a string"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_role() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("role", CborValue::Bool(false)),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`role` must be a string"), "got: {err}");
    }
}
