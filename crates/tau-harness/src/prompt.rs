//! Building blocks for the per-turn prompt: the system prompt body, the
//! AGENTS.md context message, and the conversation assembly that turns a
//! [`tau_core::SessionTree`] into item-based prompt context.

use tau_core::SessionEntry;
use tau_proto::{CborValue, ContextItem};

use crate::dedup::DEDUP_MARKER;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};

/// Builds the system prompt from available tools, skills, and cwd.
///
/// Must be deterministic and stable across turns of the same session
/// — see the linear-prefix invariant in `send_prompt_to_agent`.
/// Tools and skills are sorted by name (HashMap iteration would
/// otherwise drift). The current date is intentionally omitted:
/// including it would invalidate the prompt cache every midnight
/// UTC. cwd is threaded in from the caller so the caller owns the
/// source of truth.
pub(crate) fn build_system_prompt(
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    cwd: &str,
) -> String {
    // Tool definitions are delivered out-of-band via the provider's
    // tool-use channel, so we don't restate them here.
    let mut prompt = format!(
        "You are an expert coding assistant operating inside Tau, \
         a coding agent harness. You help users by reading files, executing commands, \
         editing code, and writing new files.\n\n\
         You can call multiple tools in a single response. \
         If you intend to call multiple tools and there are no dependencies between the calls, \
         make all independent tool calls in parallel in the same response. \
         Maximize use of parallel tool calls where possible to increase efficiency. \
         However, if some tool calls depend on previous calls to inform dependent values, \
         do NOT call these tools in parallel and instead call them sequentially. \
         For instance, if one operation must complete before another starts, \
         run these operations sequentially instead.\n\n\
         Tau deduplicates tool result outputs. `{DEDUP_MARKER}` results are \
         not errors, but an optimization pointing at an earlier identical result.\n\n",
    );

    // Available skills section.
    let mut prompt_skills: Vec<_> = skills.iter().filter(|(_, s)| s.add_to_prompt).collect();
    prompt_skills.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    if !prompt_skills.is_empty() {
        prompt.push_str(
            "\nSkills provide specialized instructions for specific tasks.\n\
             Below are skills you should be initially aware of. Use the skill tool to load a skill, and search for more skills.\n\n\
             <available_skills>\n",
        );
        for (name, skill) in &prompt_skills {
            let description = tau_skills::truncate_description(&skill.description);
            prompt.push_str(&format!(
                "  <skill>\n    <name>{}</name>\n    \
                 <description>{}</description>\n  </skill>\n",
                xml_escape(name.as_str()),
                xml_escape(description.as_ref()),
            ));
        }
        prompt.push_str("</available_skills>\n");
    }

    prompt.push_str(&format!("\nCurrent working directory: {cwd}\n"));

    prompt
}

fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) fn render_agents_context_message<'a>(
    files: impl IntoIterator<Item = &'a DiscoveredAgentsFile>,
) -> String {
    let mut text = String::from(
        "# AGENTS.md instructions\n\n\
The following instructions were loaded from AGENTS.md files.\n\
More specific files usually override broader ones.\n\n",
    );

    for file in files {
        text.push_str(&format!(
            "<AGENTS_FILE path=\"{}\">\n",
            file.file_path.display()
        ));
        text.push_str(&file.content);
        if !file.content.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("</AGENTS_FILE>\n\n");
    }

    text
}

/// Returns the current date as YYYY-MM-DD without chrono.
pub(crate) fn chrono_free_date() -> String {
    // Use UNIX timestamp to derive date.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    // Simple days-since-epoch to Y-M-D (good enough, no leap second edge cases).
    let mut y = 1970_i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for md in &month_days {
        if remaining < *md {
            break;
        }
        remaining -= md;
        m += 1;
    }
    format!("{y}-{:02}-{:02}", m + 1, remaining + 1)
}

/// Converts the branch ending at `head` into LLM prompt context
/// items. Each conversation tracks its own head; with multiple
/// side conversations interleaving tree mutations (one delegate's
/// teardown snapping `tree.head` to the default conv, another
/// delegate's tool result arriving moments later), `tree.head()` is
/// not reliable as the prompt-assembly cursor — use the conv's own
/// head instead.
pub(crate) struct AssembledPromptContext {
    pub(crate) context_items: Vec<ContextItem>,
}

pub(crate) fn assemble_conversation_from(
    tree: &tau_core::SessionTree,
    head: Option<tau_core::NodeId>,
) -> Vec<ContextItem> {
    assemble_prompt_context_from(tree, head).context_items
}

pub(crate) fn assemble_prompt_context_from(
    tree: &tau_core::SessionTree,
    head: Option<tau_core::NodeId>,
) -> AssembledPromptContext {
    let mut context_items: Vec<ContextItem> = Vec::new();

    for entry in tree.branch_from(head) {
        match entry {
            SessionEntry::UserInput { items } => {
                context_items.extend(items.iter().cloned());
            }
            SessionEntry::AssistantResponse { output_items, .. } => {
                context_items.extend(output_items.iter().cloned());
            }
            SessionEntry::ToolResults { items } => {
                context_items.extend(items.iter().cloned().map(ContextItem::ToolResult));
            }
            SessionEntry::Compaction { replacement_window } => {
                context_items = replacement_window.clone();
            }
        }
    }

    AssembledPromptContext { context_items }
}

/// Extract a boolean value from a CBOR map by key.
pub(crate) fn cbor_map_bool(map: &CborValue, key: &str) -> Option<bool> {
    match map {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Bool(b)) if k == key => Some(*b),
            _ => None,
        }),
        _ => None,
    }
}

/// Converts a CBOR value to human-readable text for tool results.
#[cfg(test)]
pub(crate) fn cbor_to_text(v: &tau_proto::CborValue) -> String {
    use tau_proto::CborValue;
    match v {
        CborValue::Null => String::new(),
        CborValue::Bool(b) => b.to_string(),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            n.to_string()
        }
        CborValue::Float(f) => f.to_string(),
        CborValue::Text(s) => s.clone(),
        CborValue::Bytes(b) => format!("<{} bytes>", b.len()),
        CborValue::Array(arr) => arr.iter().map(cbor_to_text).collect::<Vec<_>>().join("\n"),
        CborValue::Map(entries) => {
            // For maps, extract text values cleanly.
            let mut parts = Vec::new();
            for (k, val) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => cbor_to_text(other),
                };
                let value = cbor_to_text(val);
                if value.contains('\n') || key == "line-numbered content" {
                    parts.push(format!("{key}:\n{value}"));
                } else {
                    parts.push(format!("{key}: {value}"));
                }
            }
            parts.join("\n")
        }
        CborValue::Tag(_, inner) => cbor_to_text(inner),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use tau_proto::{
        ContentPart, ContextItem, ContextRole, Event, MessageItem, ToolError, ToolResultStatus,
    };

    use super::*;

    fn assistant_message(text: &str) -> ContextItem {
        ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
        })
    }

    #[test]
    fn build_system_prompt_includes_cwd() {
        let skills = std::collections::HashMap::new();
        let prompt = build_system_prompt(&skills, "/tmp/work");
        assert!(prompt.contains("expert coding assistant"));
        assert!(prompt.contains("Current working directory: /tmp/work"));
    }

    #[test]
    fn build_system_prompt_encourages_parallel_tool_calls() {
        let skills = std::collections::HashMap::new();
        let prompt = build_system_prompt(&skills, "/tmp/work");
        assert!(prompt.contains("parallel"));
        assert!(prompt.contains("make all independent tool calls in parallel"));
    }

    #[test]
    fn cbor_to_text_puts_line_numbered_content_on_next_line() {
        let text = cbor_to_text(&CborValue::Map(vec![(
            CborValue::Text("line-numbered content".to_owned()),
            CborValue::Text("1 only".to_owned()),
        )]));

        assert_eq!(text, "line-numbered content:\n1 only");
    }

    /// Tool errors must surface their `details` payload to the LLM,
    /// not just the bare `message`. The shell extension stuffs
    /// stdout/stderr/exit_code into `details` on failure; without
    /// this, the model sees only "command exited with status 1" and
    /// has to re-run the command with `2>&1 | tail` to recover the
    /// diagnostic output.
    #[test]
    fn assemble_conversation_includes_tool_error_details() {
        let mut tree = tau_core::SessionTree::from_events("session-1".into(), &[]);
        tree.apply_event(&Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            text: "build firefox".to_owned(),
            session_id: "session-1".into(),
            originator: tau_proto::PromptOriginator::default(),
            ctx_id: None,
        }));
        tree.apply_event(&Event::AgentResponseFinished(
            tau_proto::AgentResponseFinished {
                session_prompt_id: "sp-tools".into(),
                output_items: vec![ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: "call-1".into(),
                    name: tau_proto::ToolName::new("shell"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Null,
                })],
                stop_reason: tau_proto::AgentStopReason::ToolCalls,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            },
        ));
        let details = CborValue::Map(vec![
            (
                CborValue::Text("stdout".to_owned()),
                CborValue::Text("compiling".to_owned()),
            ),
            (
                CborValue::Text("stderr".to_owned()),
                CborValue::Text("patch 73cbb9ff failed to apply".to_owned()),
            ),
            (
                CborValue::Text("status".to_owned()),
                CborValue::Integer(1.into()),
            ),
        ]);
        tree.apply_event(&Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            message: "command exited with status 1".to_owned(),
            details: Some(details),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }));

        let items = assemble_conversation_from(&tree, tree.head());
        let tool_result = items
            .iter()
            .find_map(|item| match item {
                ContextItem::ToolResult(result)
                    if matches!(result.status, ToolResultStatus::Error { .. }) =>
                {
                    Some(result)
                }
                _ => None,
            })
            .expect("error tool result should be present");

        let ToolResultStatus::Error { message } = &tool_result.status else {
            panic!("expected error tool result status")
        };
        let detail_text = cbor_to_text(&tool_result.output);

        assert!(
            message.contains("command exited with status 1"),
            "missing message: {message}"
        );
        assert!(
            detail_text.contains("patch 73cbb9ff failed to apply"),
            "missing stderr: {detail_text}"
        );
        assert!(
            detail_text.contains("compiling"),
            "missing stdout: {detail_text}"
        );
    }

    /// `phase` captured on a prior assistant turn must show up on
    /// the `ConversationMessage` we hand to the backend on the next
    /// prompt. This is the link in the chain that lets the
    /// Responses backend stamp the wire field without round-tripping
    /// through a separate side channel.
    #[test]
    fn assemble_conversation_preserves_agent_phase() {
        let mut tree = tau_core::SessionTree::from_events("session-1".into(), &[]);
        tree.apply_event(&Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            text: "hi".to_owned(),
            session_id: "session-1".into(),
            originator: tau_proto::PromptOriginator::default(),
            ctx_id: None,
        }));
        tree.apply_event(&Event::AgentResponseFinished(
            tau_proto::AgentResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "draft answer".to_owned(),
                    }],
                    phase: Some(tau_proto::MessagePhase::Commentary),
                })],
                stop_reason: tau_proto::AgentStopReason::EndTurn,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            },
        ));

        let items = assemble_conversation_from(&tree, tree.head());
        let assistant = items
            .iter()
            .find_map(|item| match item {
                ContextItem::Message(message) if message.role == ContextRole::Assistant => {
                    Some(message)
                }
                _ => None,
            })
            .expect("assistant message");
        assert_eq!(assistant.phase, Some(tau_proto::MessagePhase::Commentary));
    }

    #[test]
    fn assemble_conversation_restarts_from_compacted_summary() {
        let mut tree = tau_core::SessionTree::from_events("session-1".into(), &[]);
        tree.apply_event(&Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            text: "first question".to_owned(),
            session_id: "session-1".into(),
            originator: tau_proto::PromptOriginator::default(),
            ctx_id: None,
        }));
        tree.apply_event(&Event::AgentResponseFinished(
            tau_proto::AgentResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![assistant_message("first answer")],
                stop_reason: tau_proto::AgentStopReason::EndTurn,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            },
        ));
        tree.apply_event(&Event::SessionCompacted(tau_proto::SessionCompacted {
            session_id: "session-1".into(),
            originator: tau_proto::PromptOriginator::User,
            replacement_window: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "Summary of earlier conversation:\n- User is debugging compaction\n- Keep edits focused"
                        .to_owned(),
                }],
                phase: None,
            })],
        }));
        tree.apply_event(&Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            text: "continue".to_owned(),
            session_id: "session-1".into(),
            originator: tau_proto::PromptOriginator::default(),
            ctx_id: None,
        }));

        let items = assemble_conversation_from(&tree, tree.head());
        assert_eq!(items.len(), 2, "pre-compaction history must be dropped");
        assert!(matches!(
            &items[0],
            ContextItem::Message(MessageItem { content, .. })
                if matches!(&content[0], ContentPart::Text { text }
                    if text.contains("Summary of earlier conversation:")
                        && text.contains("debugging compaction"))
        ));
        assert!(matches!(
            &items[1],
            ContextItem::Message(MessageItem { content, .. })
                if matches!(&content[0], ContentPart::Text { text } if text == "continue")
        ));
    }

    /// Encrypted-reasoning replay: when `AgentResponseFinished` carries
    /// `reasoning_items`, the next assembled prompt's assistant
    /// message must front-load them as `ContentBlock::Reasoning` blocks
    /// before any text. The responses backend then emits them as
    /// top-level `input[]` items (covered by
    /// `build_request_replays_reasoning_item_as_top_level_input`);
    /// this test pins the persistence half of that pipeline so a
    /// future fold refactor can't silently drop them on the floor.
    #[test]
    fn assemble_conversation_replays_reasoning_items_before_text() {
        let mut tree = tau_core::SessionTree::from_events("session-1".into(), &[]);
        tree.apply_event(&Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            text: "hi".to_owned(),
            session_id: "session-1".into(),
            originator: tau_proto::PromptOriginator::default(),
            ctx_id: None,
        }));
        let blob = serde_json::json!({
            "type": "reasoning",
            "id": "rs_xyz",
            "encrypted_content": "OPAQUE",
        })
        .to_string();
        tree.apply_event(&Event::AgentResponseFinished(
            tau_proto::AgentResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![
                    ContextItem::Reasoning(
                        serde_json::from_str(&blob).expect("opaque reasoning item"),
                    ),
                    assistant_message("here's what I found"),
                ],
                stop_reason: tau_proto::AgentStopReason::EndTurn,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            },
        ));

        let items = assemble_conversation_from(&tree, tree.head());
        assert!(matches!(&items[1], ContextItem::Reasoning(_)));
        assert!(matches!(
            &items[2],
            ContextItem::Message(MessageItem { content, .. })
                if matches!(&content[0], ContentPart::Text { text } if text == "here's what I found")
        ));
    }

    /// Tool-only turn (no message text) with reasoning_items must
    /// still persist as an `AgentMessage` entry — otherwise the
    /// reasoning blob would be lost and reasoning continuity breaks
    /// on any subsequent full-transcript replay. The assembled
    /// assistant message has no Text block but does have the
    /// Reasoning block, ready for the responses backend to emit it
    /// before any function_call items that follow.
    #[test]
    fn assemble_conversation_persists_reasoning_on_tool_only_turn() {
        let mut tree = tau_core::SessionTree::from_events("session-1".into(), &[]);
        tree.apply_event(&Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            text: "go".to_owned(),
            session_id: "session-1".into(),
            originator: tau_proto::PromptOriginator::default(),
            ctx_id: None,
        }));
        let blob = serde_json::json!({
            "type": "reasoning",
            "id": "rs_tool_turn",
            "encrypted_content": "OPAQUE",
        })
        .to_string();
        tree.apply_event(&Event::AgentResponseFinished(
            tau_proto::AgentResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![ContextItem::Reasoning(
                    serde_json::from_str(&blob).expect("opaque reasoning item"),
                )],
                stop_reason: tau_proto::AgentStopReason::EndTurn,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            },
        ));

        let items = assemble_conversation_from(&tree, tree.head());
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[1], ContextItem::Reasoning(_)));
    }
}
