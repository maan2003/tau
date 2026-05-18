//! Building blocks for the per-turn prompt: the system prompt body, the
//! AGENTS.md context message, and the conversation assembly that turns a
//! [`tau_core::SessionTree`] into item-based prompt context.

use tau_core::SessionEntry;
use tau_proto::{CborValue, ContextItem, PromptContent, PromptHook, PromptPriority};

use crate::dedup::DEDUP_MARKER;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};

const ROLE_EXTRA_PROMPT_PRIORITY: PromptPriority = PromptPriority::new(1000);

const FOREMAN_ROLE_PROMPT: &str = "You are a foreman/orchestrator agent. Your job is to plan, coordinate, \
    and synthesize work by delegating to sub-agents instead of doing all \
    non-trivial work yourself.\n\n\
    Default workflow:\n\n\
    * For tiny, simple, or purely clerical tasks, you may work directly.\n\
    * For non-trivial tasks, use the `delegate` tool as the default path. \
    Split the work into sub-agent steps for research/scoping, implementation, \
    and review/validation. Split or repeat steps as needed for the task scope \
    and difficulty.\n\
    * Delegate those steps instead of performing every detail yourself.\n\
    * Pass each sub-agent complete, self-contained instructions: the goal, \
    relevant context, exact paths/symbols/snippets, constraints, \
    tests/validation to run, and expected answer format.\n\
    * When useful, choose an explicit role for delegated work from the available \
    sub-task roles list.\n\
    * Synthesize sub-agent results, resolve discrepancies, do only tiny or \
    clerical follow-up directly, and report the final outcome.";

/// Return the built-in prompt text for a role, if Tau ships one.
///
/// Keeping this as a distinct layer makes configured `prompt` an override of
/// default role prompt text rather than an additive append point.
pub(crate) fn default_tau_role_prompt(role_name: &str) -> Option<PromptContent> {
    match role_name {
        "foreman" => Some(PromptContent::new(FOREMAN_ROLE_PROMPT)),
        _ => None,
    }
}

/// Resolve the role prompt layer after applying the configured override.
pub(crate) fn effective_tau_role_prompt<'a>(
    role_prompt_override: Option<&'a PromptContent>,
    default_role_prompt: Option<&'a PromptContent>,
) -> Option<&'a PromptContent> {
    role_prompt_override.or(default_role_prompt)
}

/// Builds the system prompt from Tau defaults plus role/tool prompt hooks.
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
    role_prompt: Option<&PromptContent>,
    role_extra_prompt: Option<&PromptContent>,
    available_sub_task_roles_prompt: Option<&PromptContent>,
    tool_prompt_hook: &PromptHook,
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

    compose_system_prompt(
        prompt,
        role_prompt,
        role_extra_prompt,
        available_sub_task_roles_prompt,
        tool_prompt_hook,
    )
}

fn compose_system_prompt(
    tau_system_prompt: String,
    role_prompt: Option<&PromptContent>,
    role_extra_prompt: Option<&PromptContent>,
    available_sub_task_roles_prompt: Option<&PromptContent>,
    tool_prompt_hook: &PromptHook,
) -> String {
    let mut prompt = tau_system_prompt.trim_end().to_owned();

    let mut role_extra_prompt_hook = PromptHook::new();
    if let Some(content) = role_extra_prompt
        && !content.is_empty()
    {
        role_extra_prompt_hook.insert((ROLE_EXTRA_PROMPT_PRIORITY, content.clone()));
    }

    let has_role_prompt = role_prompt.is_some_and(|content| !content.is_empty());
    let has_role_extra = !role_extra_prompt_hook.is_empty();
    let has_available_roles =
        available_sub_task_roles_prompt.is_some_and(|content| !content.is_empty());
    if has_role_prompt || has_role_extra || has_available_roles {
        prompt.push_str("\n\n");
        if let Some(content) = role_prompt
            && !content.is_empty()
        {
            prompt.push_str(content.as_str());
        }
        if has_role_prompt && has_role_extra {
            prompt.push('\n');
        }
        append_prompt_hook(&mut prompt, &role_extra_prompt_hook);
        if (has_role_prompt || has_role_extra) && has_available_roles {
            prompt.push_str("\n\n");
        }
        if let Some(content) = available_sub_task_roles_prompt
            && !content.is_empty()
        {
            prompt.push_str(content.as_str());
        }
    }

    if prompt_hook_has_content(tool_prompt_hook) {
        prompt.push_str("\n\n");
        append_prompt_hook(&mut prompt, tool_prompt_hook);
    }

    prompt.push('\n');
    prompt
}

fn prompt_hook_has_content(hook: &PromptHook) -> bool {
    hook.iter().any(|(_, content)| !content.is_empty())
}

fn append_prompt_hook(prompt: &mut String, hook: &PromptHook) {
    let mut first = true;
    for (_, content) in hook {
        if content.is_empty() {
            continue;
        }
        if !first {
            prompt.push_str("\n\n");
        }
        prompt.push_str(content.as_str());
        first = false;
    }
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
        let prompt = build_system_prompt(
            &skills,
            "/tmp/work",
            None,
            None,
            None,
            &tau_proto::PromptHook::new(),
        );
        assert!(prompt.contains("expert coding assistant"));
        assert!(prompt.contains("Current working directory: /tmp/work"));
    }

    #[test]
    fn build_system_prompt_encourages_parallel_tool_calls() {
        let skills = std::collections::HashMap::new();
        let prompt = build_system_prompt(
            &skills,
            "/tmp/work",
            None,
            None,
            None,
            &tau_proto::PromptHook::new(),
        );
        assert!(prompt.contains("parallel"));
        assert!(prompt.contains("make all independent tool calls in parallel"));
    }

    /// A configured role prompt replaces default role prompt text. If neither
    /// exists, the role prompt layer is absent rather than represented as an
    /// empty append-only section.
    #[test]
    fn effective_tau_role_prompt_uses_override_else_default_else_none() {
        let configured = tau_proto::PromptContent::new("CONFIGURED ROLE PROMPT");
        let default = tau_proto::PromptContent::new("DEFAULT ROLE PROMPT");

        assert_eq!(
            effective_tau_role_prompt(Some(&configured), Some(&default))
                .map(tau_proto::PromptContent::as_str),
            Some("CONFIGURED ROLE PROMPT")
        );
        assert_eq!(
            effective_tau_role_prompt(None, Some(&default)).map(tau_proto::PromptContent::as_str),
            Some("DEFAULT ROLE PROMPT")
        );
        assert!(effective_tau_role_prompt(None, None).is_none());
        assert!(default_tau_role_prompt("smart").is_none());
        let foreman = default_tau_role_prompt("foreman").expect("foreman prompt");
        assert!(
            foreman
                .as_str()
                .contains("You are a foreman/orchestrator agent")
        );
        assert!(foreman.as_str().contains("use the `delegate` tool"));
        assert!(
            foreman
                .as_str()
                .contains("research/scoping, implementation")
        );
        assert!(foreman.as_str().contains("self-contained instructions"));
        assert!(foreman.as_str().contains("available sub-task roles list"));
    }

    /// Orchestrator roles append the available sub-task roles after the
    /// effective role prompt so user prompt overrides do not hide delegation
    /// choices from the model.
    #[test]
    fn build_system_prompt_appends_available_roles_for_orchestrator_context() {
        let skills = std::collections::HashMap::new();
        let role_prompt = tau_proto::PromptContent::new("CUSTOM FOREMAN");
        let available_roles = tau_proto::PromptContent::new(
            "## Available sub-task roles\n\n* `smart` - \"Individual contributor using state of the art model. Good default for most tasks.\"",
        );

        let prompt = build_system_prompt(
            &skills,
            "/tmp/work",
            Some(&role_prompt),
            None,
            Some(&available_roles),
            &tau_proto::PromptHook::new(),
        );

        let role = prompt.find("CUSTOM FOREMAN").expect("role prompt");
        let roles = prompt
            .find("## Available sub-task roles")
            .expect("available roles");
        assert!(role < roles);
        assert!(prompt.contains("* `smart` - \"Individual contributor using state of the art model. Good default for most tasks.\""));
    }

    /// Role prompt overrides, role extra prompts, and tool prompt hooks are
    /// distinct layers. This pins their composition order so adding more hook
    /// contributors later does not accidentally move tool instructions before
    /// role instructions or ignore hook priority ordering.
    #[test]
    fn build_system_prompt_composes_role_and_tool_prompt_hooks_in_order() {
        let skills = std::collections::HashMap::new();
        let role_prompt = tau_proto::PromptContent::new("ROLE PROMPT");
        let role_extra = tau_proto::PromptContent::new("ROLE EXTRA");
        let mut tool_hook = tau_proto::PromptHook::new();
        tool_hook.insert((
            tau_proto::PromptPriority::new(20),
            tau_proto::PromptContent::new("TOOL LATE"),
        ));
        tool_hook.insert((
            tau_proto::PromptPriority::new(10),
            tau_proto::PromptContent::new("TOOL EARLY"),
        ));

        let prompt = build_system_prompt(
            &skills,
            "/tmp/work",
            Some(&role_prompt),
            Some(&role_extra),
            None,
            &tool_hook,
        );

        let base = prompt
            .find("Current working directory: /tmp/work")
            .expect("base Tau system prompt should render cwd");
        let role = prompt
            .find("ROLE PROMPT")
            .expect("role prompt should be rendered");
        let extra = prompt
            .find("ROLE EXTRA")
            .expect("role extra prompt should be rendered");
        let early = prompt
            .find("TOOL EARLY")
            .expect("earlier-priority tool prompt should be rendered");
        let late = prompt
            .find("TOOL LATE")
            .expect("later-priority tool prompt should be rendered");
        assert!(base < role);
        assert!(role < extra);
        assert!(extra < early);
        assert!(early < late);
    }

    /// Empty hook entries are ignored without adding a blank prompt section.
    #[test]
    fn build_system_prompt_ignores_empty_tool_prompt_hook_sections() {
        let skills = std::collections::HashMap::new();
        let without_hook = build_system_prompt(
            &skills,
            "/tmp/work",
            None,
            None,
            None,
            &tau_proto::PromptHook::new(),
        );
        let mut empty_hook = tau_proto::PromptHook::new();
        empty_hook.insert((
            tau_proto::PromptPriority::new(10),
            tau_proto::PromptContent::new(""),
        ));
        let with_empty_hook =
            build_system_prompt(&skills, "/tmp/work", None, None, None, &empty_hook);

        assert_eq!(with_empty_hook, without_hook);
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
        tree.apply_event(&Event::ProviderResponseFinished(
            tau_proto::ProviderResponseFinished {
                session_prompt_id: "sp-tools".into(),
                output_items: vec![ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: "call-1".into(),
                    name: tau_proto::ToolName::new("shell"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Null,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
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
        tree.apply_event(&Event::ProviderResponseFinished(
            tau_proto::ProviderResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "draft answer".to_owned(),
                    }],
                    phase: Some(tau_proto::MessagePhase::Commentary),
                })],
                stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        tree.apply_event(&Event::ProviderResponseFinished(
            tau_proto::ProviderResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![assistant_message("first answer")],
                stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
            original_input_tokens: None,
            compacted_input_tokens: None,
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

    /// Encrypted-reasoning replay: when `ProviderResponseFinished` carries
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
        tree.apply_event(&Event::ProviderResponseFinished(
            tau_proto::ProviderResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![
                    ContextItem::Reasoning(
                        serde_json::from_str(&blob).expect("opaque reasoning item"),
                    ),
                    assistant_message("here's what I found"),
                ],
                stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
        tree.apply_event(&Event::ProviderResponseFinished(
            tau_proto::ProviderResponseFinished {
                session_prompt_id: "sp-1".into(),
                output_items: vec![ContextItem::Reasoning(
                    serde_json::from_str(&blob).expect("opaque reasoning item"),
                )],
                stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
