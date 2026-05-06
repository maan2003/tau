//! Building blocks for the per-turn prompt: the system prompt body, the
//! AGENTS.md context message, and the conversation assembly that turns a
//! [`tau_core::SessionTree`] into provider-shaped [`ConversationMessage`]s.

use tau_core::{SessionEntry, ToolActivityOutcome};
use tau_proto::{CborValue, ContentBlock, ConversationMessage, ConversationRole, ToolDefinition};

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
    tools: &[ToolDefinition],
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    cwd: &str,
) -> String {
    let mut prompt = String::from(
        "You are an expert coding assistant operating inside Tau, \
         a coding agent harness. TAU_VERSION contains Tau's release \
         version, and TAU_BUILD contains Tau's git revision for this \
         build. You help users by reading files, executing commands, \
         editing code, and writing new files.\n\n",
    );

    // Available tools section.
    if !tools.is_empty() {
        prompt.push_str("Available tools:\n");
        for tool in tools {
            let desc = tool.description.as_deref().unwrap_or("(no description)");
            prompt.push_str(&format!("- {}: {desc}\n", tool.name));
        }
        prompt.push('\n');
    }

    // Guidelines.
    prompt.push_str(
        "Guidelines:\n\
         - Be concise in your responses.\n\
         - Show file paths clearly when working with files.\n\
         - When asked to read a file, use the read tool.\n\
         - When asked to run a command, use the shell tool.\n",
    );

    // Available skills section.
    let mut prompt_skills: Vec<_> = skills.iter().filter(|(_, s)| s.add_to_prompt).collect();
    prompt_skills.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    if !prompt_skills.is_empty() {
        prompt.push_str(
            "\nThe following skills provide specialized instructions for specific tasks.\n\
             Use the skill tool to load a skill when the task matches its description.\n\n\
             <available_skills>\n",
        );
        for (name, skill) in &prompt_skills {
            prompt.push_str(&format!(
                "  <skill>\n    <name>{name}</name>\n    \
                 <description>{}</description>\n  </skill>\n",
                skill.description
            ));
        }
        prompt.push_str("</available_skills>\n");
    }

    prompt.push_str(&format!("\nCurrent working directory: {cwd}\n"));

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_system_prompt_mentions_tau_version_and_build() {
        let skills = std::collections::HashMap::new();
        let prompt = build_system_prompt(&[], &skills, "/tmp/work");
        assert!(prompt.contains("TAU_VERSION contains Tau's release version"));
        assert!(prompt.contains("TAU_BUILD contains Tau's git revision"));
    }
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

/// Converts the branch ending at `head` into LLM conversation
/// messages. Each conversation tracks its own head; with multiple
/// side conversations interleaving tree mutations (one delegate's
/// teardown snapping `tree.head` to the default conv, another
/// delegate's tool result arriving moments later), `tree.head()` is
/// not reliable as the prompt-assembly cursor — use the conv's own
/// head instead.
pub(crate) fn assemble_conversation_from(
    tree: &tau_core::SessionTree,
    head: Option<tau_core::NodeId>,
) -> Vec<ConversationMessage> {
    let mut messages: Vec<ConversationMessage> = Vec::new();

    for entry in tree.branch_from(head) {
        match entry {
            SessionEntry::UserMessage { text } => {
                messages.push(ConversationMessage {
                    role: ConversationRole::User,
                    content: vec![ContentBlock::Text { text: text.clone() }],
                });
            }
            SessionEntry::AgentMessage { text, thinking: _ } => {
                // `thinking` is intentionally NOT replayed: provider
                // reasoning summaries are for human inspection only,
                // never fed back into later turns as plain assistant
                // text. See `TAU_VISIBLE_THINKING_IMPLEMENTATION_PLAN.md`.
                messages.push(ConversationMessage {
                    role: ConversationRole::Assistant,
                    content: vec![ContentBlock::Text { text: text.clone() }],
                });
            }
            SessionEntry::ToolActivity(activity) => match &activity.outcome {
                ToolActivityOutcome::Requested { arguments } => {
                    // Tool use goes into the preceding assistant message.
                    // If there's no assistant message yet, create one.
                    let needs_new = messages
                        .last()
                        .is_none_or(|m| m.role != ConversationRole::Assistant);
                    if needs_new {
                        messages.push(ConversationMessage {
                            role: ConversationRole::Assistant,
                            content: Vec::new(),
                        });
                    }
                    if let Some(last) = messages.last_mut() {
                        last.content.push(ContentBlock::ToolUse {
                            id: activity.call_id.clone(),
                            name: activity.tool_name.clone().into(),
                            input: arguments.clone(),
                        });
                    }
                }
                ToolActivityOutcome::Result { result } => {
                    messages.push(ConversationMessage {
                        role: ConversationRole::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: activity.call_id.clone(),
                            content: cbor_to_text(result),
                            is_error: false,
                        }],
                    });
                }
                ToolActivityOutcome::Error { message, .. } => {
                    messages.push(ConversationMessage {
                        role: ConversationRole::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: activity.call_id.clone(),
                            content: message.clone(),
                            is_error: true,
                        }],
                    });
                }
            },
        }
    }

    messages
}

/// Extract a string value from a CBOR map by key.
pub(crate) fn cbor_map_text<'a>(map: &'a CborValue, key: &str) -> Option<&'a str> {
    match map {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Text(v)) if k == key => Some(v.as_str()),
            _ => None,
        }),
        _ => None,
    }
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
                if value.contains('\n') {
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
