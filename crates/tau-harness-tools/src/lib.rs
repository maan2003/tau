//! Built-in internal tools for `tau-harness`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tau_harness::internal_tools::{InternalSkill, InternalSkillSource};
use tau_harness::{
    AgentToolCall, ConversationId, HarnessError, InternalToolHandler, InternalToolHost,
};
use tau_proto::{
    BackgroundSupport, CborValue, Event, PromptOriginator, StartAgentRequest, ToolCallId,
    ToolDisplay, ToolDisplayStats, ToolDisplayStatus, ToolError, ToolExecutionMode, ToolName,
    ToolResult, ToolResultKind, ToolSpec, ToolStarted, ToolType,
};

const SKILL_TOOL_NAME: &str = "skill";
const DELEGATE_TOOL_NAME: &str = "delegate";
const WAIT_TOOL_NAME: &str = "wait";
const CANCEL_TOOL_NAME: &str = "cancel";
const MESSAGE_TOOL_NAME: &str = "message";
const DELEGATE_PREFIX: &str =
    include_str!("../../tau-harness/src/harness/prompts/delegate_prefix.md");
const SLOW_DELEGATE_EXEC_TIME_THRESHOLD_SECS: u64 = 5;

/// Return handlers for Tau's built-in harness-process tools.
pub fn builtin_handlers() -> Vec<Arc<dyn InternalToolHandler>> {
    vec![Arc::new(BuiltinTools::default())]
}

#[derive(Default)]
struct BuiltinTools {
    state: Mutex<BuiltinState>,
}

#[derive(Default)]
struct BuiltinState {
    pending_delegates: HashMap<String, PendingDelegate>,
    cancel_requested: HashSet<ToolCallId>,
    next_delegate_query_id: u64,
}

struct PendingDelegate {
    call_id: ToolCallId,
    tool_name: ToolName,
    started_at: Instant,
    self_agent_id: String,
    agent_id: String,
}

impl InternalToolHandler for BuiltinTools {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![
            skill_tool_spec(),
            delegate_tool_spec(),
            wait_tool_spec(),
            cancel_tool_spec(),
            message_tool_spec(),
        ]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        matches!(
            internal_tool_name.as_str(),
            SKILL_TOOL_NAME
                | DELEGATE_TOOL_NAME
                | WAIT_TOOL_NAME
                | CANCEL_TOOL_NAME
                | MESSAGE_TOOL_NAME
        )
    }

    fn handle_event(
        &self,
        host: &mut InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        match event {
            Event::ToolStarted(started) => {
                let Some((conversation_id, call, visible_tool_name)) = started_call(host, started)
                else {
                    return Ok(());
                };
                match call.name.as_str() {
                    SKILL_TOOL_NAME => {
                        handle_skill_tool_call(host, &conversation_id, &call, visible_tool_name)
                    }
                    DELEGATE_TOOL_NAME => self.handle_delegate_tool_call(
                        host,
                        &conversation_id,
                        &call,
                        visible_tool_name,
                    ),
                    WAIT_TOOL_NAME => {
                        host.handle_wait_tool_call(&conversation_id, &call, visible_tool_name)
                    }
                    MESSAGE_TOOL_NAME => {
                        handle_message_tool_call(host, &conversation_id, &call, visible_tool_name)
                    }
                    CANCEL_TOOL_NAME => self.handle_cancel_tool_call(
                        host,
                        &conversation_id,
                        &call,
                        visible_tool_name,
                    ),
                    _ => Ok(()),
                }
            }
            Event::StartAgentResult(result) => self.handle_start_agent_result(host, result),
            Event::ToolCancelRequest(request) => {
                self.handle_tool_cancel_request(host, &request.target_call_id)
            }
            Event::StartAgentAccepted(_) => Ok(()),
            _ => Ok(()),
        }
    }
}

impl BuiltinTools {
    fn handle_delegate_tool_call(
        &self,
        host: &mut InternalToolHost<'_>,
        cid: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id = call.id.clone();
        host.ensure_internal_tool_tracking(cid, call, &visible_tool_name);
        let parsed = match parse_delegate_args(&call.arguments) {
            Ok(parsed) => parsed,
            Err(message) => {
                host.finish_tool_with_error(
                    cid,
                    call_id,
                    visible_tool_name,
                    call.tool_type,
                    message,
                    Some(call.arguments.clone()),
                );
                return Ok(());
            }
        };
        let Some(self_agent_id) = host.ensure_agent_id_for_conversation(cid) else {
            host.finish_tool_with_error(
                cid,
                call_id,
                visible_tool_name,
                call.tool_type,
                "sender conversation no longer exists".to_owned(),
                Some(call.arguments.clone()),
            );
            return Ok(());
        };
        let role_for_id = parsed.role.as_deref().unwrap_or("engineer");
        let agent_id = host.mint_agent_id_for_role(role_for_id);
        let query_id = {
            let mut state = self.state.lock().expect("builtin tool state poisoned");
            let query_id = format!("delegate-{}", state.next_delegate_query_id);
            state.next_delegate_query_id += 1;
            state.pending_delegates.insert(
                query_id.clone(),
                PendingDelegate {
                    call_id: call_id.clone(),
                    tool_name: visible_tool_name.clone(),
                    started_at: Instant::now(),
                    self_agent_id: self_agent_id.clone(),
                    agent_id: agent_id.clone(),
                },
            );
            query_id
        };
        let start_request = StartAgentRequest {
            query_id: query_id.clone(),
            agent_id: agent_id.clone(),
            instruction: format!("{DELEGATE_PREFIX}{}", parsed.prompt),
            role: parsed.role,
            execution_mode: parsed.execution_mode,
            input_stats: ToolDisplayStats::for_text(&parsed.prompt),
            tool_call_id: Some(call_id.clone()),
            task_name: Some(parsed.task_name),
        };
        if let Err(message) = host.enqueue_start_agent_request_without_draining(start_request) {
            self.state
                .lock()
                .expect("builtin tool state poisoned")
                .pending_delegates
                .remove(&query_id);
            host.finish_tool_with_error(
                cid,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            );
            return Ok(());
        }
        if host.mark_tool_backgrounded(&call_id) {
            host.publish_background_placeholder(
                &call_id,
                CborValue::Text(delegate_background_placeholder(
                    &call_id,
                    &self_agent_id,
                    &agent_id,
                )),
            );
        }
        host.drain_start_agent_requests()
    }

    fn handle_tool_cancel_request(
        &self,
        host: &mut InternalToolHost<'_>,
        target_call_id: &ToolCallId,
    ) -> Result<(), HarnessError> {
        let query_id = self
            .state
            .lock()
            .expect("builtin tool state poisoned")
            .pending_delegates
            .iter()
            .find_map(|(query_id, pending)| {
                (&pending.call_id == target_call_id).then(|| query_id.clone())
            });
        if let Some(query_id) = query_id {
            let _ = host.cancel_start_agent_request(&query_id, target_call_id, false);
        }
        Ok(())
    }

    fn handle_start_agent_result(
        &self,
        host: &mut InternalToolHost<'_>,
        result: &tau_proto::StartAgentResult,
    ) -> Result<(), HarnessError> {
        let Some(pending) = self
            .state
            .lock()
            .expect("builtin tool state poisoned")
            .pending_delegates
            .remove(&result.query_id)
        else {
            return Ok(());
        };
        let duration_seconds = delegate_duration_seconds(pending.started_at.elapsed());
        if let Some(message) = result.error.clone() {
            host.finish_prebuilt_tool_error(ToolError {
                call_id: pending.call_id,
                tool_name: pending.tool_name,
                tool_type: ToolType::Function,
                message,
                details: delegate_error_details(
                    duration_seconds,
                    Some(&pending.self_agent_id),
                    Some(&pending.agent_id),
                ),
                display: None,
                originator: PromptOriginator::User,
            });
        } else {
            host.finish_prebuilt_tool_result(ToolResult {
                call_id: pending.call_id,
                tool_name: pending.tool_name,
                tool_type: ToolType::Function,
                result: delegate_result_value(
                    result.text.clone(),
                    duration_seconds,
                    Some(&pending.self_agent_id),
                    Some(&pending.agent_id),
                ),
                kind: ToolResultKind::Final,
                display: None,
                originator: PromptOriginator::User,
            });
        }
        Ok(())
    }
}

fn started_call(
    host: &mut InternalToolHost<'_>,
    started: &ToolStarted,
) -> Option<(ConversationId, AgentToolCall, ToolName)> {
    host.internal_started_call(started)
}

const MAX_SKILL_CONTENT_BYTES: usize = 64 * 1024;
const MAX_SKILL_SEARCH_MATCHES: usize = 50;

fn handle_skill_tool_call(
    host: &mut InternalToolHost<'_>,
    conversation_id: &ConversationId,
    call: &AgentToolCall,
    visible_tool_name: ToolName,
) -> Result<(), HarnessError> {
    let call_id = call.id.clone();
    host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
    match handle_skill_query(host, &call.arguments) {
        Ok((result, display)) => host.finish_tool_with_cbor_result(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            result,
            display,
        ),
        Err((message, display)) => host.finish_tool_with_display_error(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            message,
            Some(call.arguments.clone()),
            display,
        ),
    }
    Ok(())
}

fn handle_skill_query(
    host: &mut InternalToolHost<'_>,
    arguments: &CborValue,
) -> Result<(CborValue, Option<ToolDisplay>), (String, Option<ToolDisplay>)> {
    let needles = extract_skill_search_queries(arguments).map_err(|message| {
        (
            message.clone(),
            Some(skill_error_display("search:", &message)),
        )
    })?;
    let search_content = extract_optional_bool(arguments, "search_content")
        .map_err(|message| {
            (
                message.clone(),
                Some(skill_error_display("search:", &message)),
            )
        })?
        .unwrap_or(false);
    let skills = host.discovered_skills();
    let outcome = search_discovered_skills(&skills, &needles, search_content);
    for warning in &outcome.warnings {
        host.emit_info_important(warning);
    }
    if let Some(name) = outcome.auto_load_name.clone() {
        return read_skill_by_name(host, &skills, &name);
    }
    Ok(skill_search_result(&needles, search_content, outcome))
}

fn read_skill_by_name(
    host: &mut InternalToolHost<'_>,
    skills: &[InternalSkill],
    name: &str,
) -> Result<(CborValue, Option<ToolDisplay>), (String, Option<ToolDisplay>)> {
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        let message = format!("unknown skill: {name}");
        return Err((message.clone(), Some(skill_error_display(name, &message))));
    };
    let source_label = skill.source.label();
    let read = read_skill_source_prefix(&skill.source, MAX_SKILL_CONTENT_BYTES).map_err(|e| {
        let message = format!("failed to read skill file: {e}");
        (message.clone(), Some(skill_error_display(name, &message)))
    })?;
    let mut body = skill_body_from_prefix(&read)
        .map_err(|message| (message.clone(), Some(skill_error_display(name, &message))))?;
    if read.truncated {
        host.emit_info_important(&format!(
            "skill too long: {source_label} truncated to {MAX_SKILL_CONTENT_BYTES} bytes while loading {name}",
        ));
        body.push_str(&format!(
            "\n\n[skill content truncated at {MAX_SKILL_CONTENT_BYTES} bytes; file has {} bytes]",
            read.total_bytes
        ));
    }
    let mut display = skill_ok_display(name);
    display.stats = text_stats_for_skill(&body);
    Ok((
        CborValue::Map(vec![
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text(name.to_owned()),
            ),
            (
                CborValue::Text("description".to_owned()),
                CborValue::Text(skill.description.clone()),
            ),
            (CborValue::Text("content".to_owned()), CborValue::Text(body)),
            (
                CborValue::Text("truncated".to_owned()),
                CborValue::Bool(read.truncated),
            ),
            (
                CborValue::Text("total_bytes".to_owned()),
                CborValue::Integer(read.total_bytes.into()),
            ),
        ]),
        Some(display),
    ))
}

struct SkillSearchHit {
    matched_terms: usize,
    matched_fields: Vec<String>,
    name: String,
    description: String,
}
struct SkillSearchOutcome {
    hits: Vec<SkillSearchHit>,
    total_matches: usize,
    truncated: bool,
    auto_load_name: Option<String>,
    warnings: Vec<String>,
}

fn search_discovered_skills(
    skills: &[InternalSkill],
    needles: &[String],
    search_content: bool,
) -> SkillSearchOutcome {
    let mut warnings = Vec::new();
    let mut hits = Vec::new();
    let mut total_matches = 0;
    let mut only_hit_name = None;
    let mut exact_hit_name = None;
    for skill in skills {
        let lower_name = skill.name.to_lowercase();
        let lower_desc = skill.description.to_lowercase();
        let mut body: Option<String> = None;
        let mut matched_fields = Vec::new();
        let mut matched_terms = 0;
        for needle in needles {
            let mut matched = false;
            if lower_name.contains(needle) {
                matched = true;
                push_matched_field(&mut matched_fields, "name");
            }
            if lower_desc.contains(needle) {
                matched = true;
                push_matched_field(&mut matched_fields, "description");
            }
            if search_content {
                let body = body.get_or_insert_with(|| match read_skill_source_prefix(&skill.source, MAX_SKILL_CONTENT_BYTES) {
                    Ok(read) => match skill_body_from_prefix(&read) {
                        Ok(body) => { if read.truncated { warnings.push(format!("skill too long: {} truncated to {MAX_SKILL_CONTENT_BYTES} bytes while content-searching {}", skill.source.label(), skill.name)); } body.to_lowercase() }
                        Err(message) => { warnings.push(format!("skill frontmatter too long: {} while content-searching {}: {message}", skill.source.label(), skill.name)); String::new() }
                    },
                    Err(_) => String::new(),
                });
                if body.contains(needle) {
                    matched = true;
                    push_matched_field(&mut matched_fields, "content");
                }
            }
            if matched {
                matched_terms += 1;
            }
        }
        if matched_terms == 0 {
            continue;
        }
        total_matches += 1;
        only_hit_name = if total_matches == 1 {
            Some(skill.name.clone())
        } else {
            None
        };
        if needles.len() == 1 && skill.name == needles[0] {
            exact_hit_name = Some(skill.name.clone());
        }
        hits.push(SkillSearchHit {
            matched_terms,
            matched_fields,
            name: skill.name.clone(),
            description: tau_skills::truncate_description(&skill.description).into_owned(),
        });
        sort_skill_hits(&mut hits);
        if MAX_SKILL_SEARCH_MATCHES < hits.len() {
            hits.truncate(MAX_SKILL_SEARCH_MATCHES);
        }
    }
    SkillSearchOutcome {
        hits,
        total_matches,
        truncated: MAX_SKILL_SEARCH_MATCHES < total_matches,
        auto_load_name: if total_matches == 1 {
            only_hit_name
        } else {
            exact_hit_name
        },
        warnings,
    }
}

fn skill_search_result(
    needles: &[String],
    search_content: bool,
    outcome: SkillSearchOutcome,
) -> (CborValue, Option<ToolDisplay>) {
    let scope_label = if search_content { " [content]" } else { "" };
    let display_args = format!("{}{scope_label}", needles.join(" "));
    let mut display = skill_ok_display(&display_args);
    display.stats = skill_search_stats(&outcome.hits);
    if outcome.truncated {
        display.status_text = format!(
            "ok: showing {} of {} matches",
            outcome.hits.len(),
            outcome.total_matches
        );
    }
    let total_matches = outcome.total_matches;
    let truncated = outcome.truncated;
    let matches = CborValue::Array(
        outcome
            .hits
            .into_iter()
            .map(|hit| {
                CborValue::Map(vec![
                    (
                        CborValue::Text("name".to_owned()),
                        CborValue::Text(hit.name),
                    ),
                    (
                        CborValue::Text("description".to_owned()),
                        CborValue::Text(hit.description),
                    ),
                    (
                        CborValue::Text("matched_terms".to_owned()),
                        CborValue::Integer((hit.matched_terms as u64).into()),
                    ),
                    (
                        CborValue::Text("matched_fields".to_owned()),
                        CborValue::Array(
                            hit.matched_fields
                                .into_iter()
                                .map(CborValue::Text)
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect(),
    );
    (
        CborValue::Map(vec![
            (
                CborValue::Text("queries".to_owned()),
                CborValue::Array(needles.iter().cloned().map(CborValue::Text).collect()),
            ),
            (
                CborValue::Text("search_content".to_owned()),
                CborValue::Bool(search_content),
            ),
            (CborValue::Text("matches".to_owned()), matches),
            (
                CborValue::Text("total_matches".to_owned()),
                CborValue::Integer((total_matches as u64).into()),
            ),
            (
                CborValue::Text("truncated".to_owned()),
                CborValue::Bool(truncated),
            ),
            (
                CborValue::Text("guidance".to_owned()),
                CborValue::Text(skill_search_guidance(total_matches)),
            ),
        ]),
        Some(display),
    )
}

struct LimitedTextRead {
    text: String,
    truncated: bool,
    total_bytes: u64,
}
fn skill_body_from_prefix(read: &LimitedTextRead) -> Result<String, String> {
    if read.truncated && tau_skills::has_unclosed_frontmatter(&read.text) {
        return Err(format!(
            "frontmatter closing fence was not found before the {MAX_SKILL_CONTENT_BYTES} byte read limit; file has {} bytes",
            read.total_bytes
        ));
    }
    Ok(tau_skills::strip_frontmatter(&read.text).to_owned())
}
fn read_skill_source_prefix(
    source: &InternalSkillSource,
    max_bytes: usize,
) -> std::io::Result<LimitedTextRead> {
    match source {
        InternalSkillSource::File(path) => read_text_file_prefix(path, max_bytes),
        InternalSkillSource::BuiltIn { content } => {
            Ok(read_text_prefix(content.as_ref(), max_bytes))
        }
    }
}
fn read_text_file_prefix(
    path: &std::path::Path,
    max_bytes: usize,
) -> std::io::Result<LimitedTextRead> {
    let mut file = std::fs::File::open(path)?;
    let total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = max_bytes < bytes.len();
    if truncated {
        bytes.truncate(max_bytes);
    }
    Ok(LimitedTextRead {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
        total_bytes,
    })
}
fn read_text_prefix(text: &str, max_bytes: usize) -> LimitedTextRead {
    let total_bytes = text.len() as u64;
    if text.len() <= max_bytes {
        return LimitedTextRead {
            text: text.to_owned(),
            truncated: false,
            total_bytes,
        };
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    LimitedTextRead {
        text: text[..end].to_owned(),
        truncated: true,
        total_bytes,
    }
}
fn sort_skill_hits(hits: &mut [SkillSearchHit]) {
    hits.sort_by(|a, b| {
        b.matched_terms
            .cmp(&a.matched_terms)
            .then_with(|| a.name.cmp(&b.name))
    });
}
fn skill_ok_display(args: &str) -> ToolDisplay {
    ToolDisplay {
        args: args.to_owned(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}
fn skill_error_display(args: &str, message: &str) -> ToolDisplay {
    ToolDisplay {
        args: args.to_owned(),
        status: ToolDisplayStatus::Error,
        status_text: error_chip_text(message),
        ..Default::default()
    }
}
fn text_stats_for_skill(text: &str) -> ToolDisplayStats {
    if text.is_empty() {
        return ToolDisplayStats::default();
    }
    ToolDisplayStats {
        matches: None,
        lines: Some(text.lines().count() as u64),
        bytes: Some(text.len() as u64),
    }
}
fn skill_search_stats(matches: &[SkillSearchHit]) -> ToolDisplayStats {
    let output = matches
        .iter()
        .map(|hit| format!("{}: {}", hit.name, hit.description))
        .collect::<Vec<_>>()
        .join("\n");
    let mut stats = text_stats_for_skill(&output);
    stats.matches = Some(matches.len() as u64);
    stats
}
fn skill_search_guidance(total_matches: usize) -> String {
    if total_matches == 0 {
        return "No skills matched. Try different terms, fewer terms, or set search_content: true if the body may mention the topic.".to_owned();
    }
    "Call `skill` again with only an exact `name` to load a specific match, or narrow `query` with a more distinctive term. Multi-term queries use OR semantics and rank by matched term count, so adding generic terms may not reduce matches. The top match was not auto-loaded because other matches also existed.".to_owned()
}
fn push_matched_field(fields: &mut Vec<String>, field: &str) {
    if !fields.iter().any(|existing| existing == field) {
        fields.push(field.to_owned());
    }
}
fn error_chip_text(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}
fn extract_skill_search_queries(arguments: &CborValue) -> Result<Vec<String>, String> {
    let raw = cbor_map_field(arguments, "query")
        .ok_or_else(|| "missing required argument: query".to_owned())?;
    let CborValue::Text(raw_query) = raw else {
        return Err("query must be a string".to_owned());
    };
    let needles = normalized_skill_query_terms(raw_query);
    if needles.is_empty() {
        return Err("query must include at least one non-empty term".to_owned());
    }
    Ok(needles)
}
fn extract_optional_bool(arguments: &CborValue, key: &str) -> Result<Option<bool>, String> {
    let Some(value) = cbor_map_field(arguments, key) else {
        return Ok(None);
    };
    let CborValue::Bool(value) = value else {
        return Err(format!("{key} must be a boolean"));
    };
    Ok(Some(*value))
}
fn cbor_map_field<'a>(arguments: &'a CborValue, key: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match k {
        CborValue::Text(k) if k == key => Some(v),
        _ => None,
    })
}
fn normalized_skill_query_terms(raw_query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in raw_query.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() || ch == '-' {
            current.push(ch);
        } else {
            push_normalized_skill_term(&mut terms, &mut current);
        }
    }
    push_normalized_skill_term(&mut terms, &mut current);
    terms
}
fn push_normalized_skill_term(terms: &mut Vec<String>, current: &mut String) {
    let term = current.trim_matches('-');
    if !term.is_empty() && !terms.iter().any(|existing| existing == term) {
        terms.push(term.to_owned());
    }
    current.clear();
}

fn handle_message_tool_call(
    host: &mut InternalToolHost<'_>,
    conversation_id: &ConversationId,
    call: &AgentToolCall,
    visible_tool_name: ToolName,
) -> Result<(), HarnessError> {
    let call_id = call.id.clone();
    host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
    let result = parse_message_args(&call.arguments).and_then(|parsed| {
        host.publish_agent_message(conversation_id, parsed.recipient_id, parsed.message)
    });
    match result {
        Ok(()) => host.finish_tool_with_result(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            "Message sent".to_owned(),
            None,
        ),
        Err(message) => host.finish_tool_with_error(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            message,
            Some(call.arguments.clone()),
        ),
    }
    Ok(())
}

impl BuiltinTools {
    fn handle_cancel_tool_call(
        &self,
        host: &mut InternalToolHost<'_>,
        conversation_id: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id = call.id.clone();
        host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
        let result = parse_cancel_args(&call.arguments).and_then(|target| {
            if !host.is_running_tool_call(&target) {
                return Err("Tool call is not running".to_owned());
            }
            let mut state = self.state.lock().expect("builtin tool state poisoned");
            if !state.cancel_requested.insert(target.clone()) {
                return Err("Tool call cancellation already requested".to_owned());
            }
            drop(state);
            host.publish_tool_cancel_request(target);
            Ok(())
        });
        match result {
            Ok(()) => host.finish_tool_with_result(
                conversation_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                "Tool cancellation requested".to_owned(),
                None,
            ),
            Err(message) => host.finish_tool_with_error(
                conversation_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            ),
        }
        Ok(())
    }
}

struct MessageArgs {
    recipient_id: String,
    message: String,
}

fn parse_message_args(arguments: &CborValue) -> Result<MessageArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut recipient_id = None;
    let mut message = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "recipient_id" => match v {
                CborValue::Text(text) => recipient_id = Some(text.clone()),
                _ => return Err("`recipient_id` must be a string".to_owned()),
            },
            "message" => match v {
                CborValue::Text(text) => message = Some(text.clone()),
                _ => return Err("`message` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let recipient_id = recipient_id.ok_or_else(|| "`recipient_id` is required".to_owned())?;
    if recipient_id.trim().is_empty() {
        return Err("`recipient_id` must not be empty".to_owned());
    }
    let message = message.ok_or_else(|| "`message` is required".to_owned())?;
    if message.trim().is_empty() {
        return Err("`message` must not be empty".to_owned());
    }
    Ok(MessageArgs {
        recipient_id,
        message,
    })
}

#[derive(Debug)]
struct DelegateArgs {
    task_name: String,
    prompt: String,
    execution_mode: ToolExecutionMode,
    role: Option<String>,
}

fn parse_delegate_args(arguments: &CborValue) -> Result<DelegateArgs, String> {
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
                CborValue::Text(text) if text == "update" => {
                    execution_mode = Some(ToolExecutionMode::Update)
                }
                CborValue::Text(text) if text == "exclusive" => {
                    execution_mode = Some(ToolExecutionMode::Exclusive)
                }
                CborValue::Text(_) => {
                    return Err(
                        "`execution_mode` must be `shared`, `update`, or `exclusive`".to_owned(),
                    );
                }
                _ => return Err("`execution_mode` must be a string".to_owned()),
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
    Ok(DelegateArgs {
        task_name,
        prompt,
        execution_mode: execution_mode.unwrap_or(ToolExecutionMode::Shared),
        role: role.filter(|role| !role.trim().is_empty()),
    })
}

fn delegate_background_placeholder(
    call_id: &ToolCallId,
    self_agent_id: &str,
    sub_agent_id: &str,
) -> String {
    format!(
        "{}: true\nself_agent_id: {self_agent_id}\nsub_agent_id: {sub_agent_id}\n\nTool call `{call_id}` is running in the background.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn delegate_duration_seconds(elapsed: Duration) -> Option<u64> {
    if Duration::from_secs(SLOW_DELEGATE_EXEC_TIME_THRESHOLD_SECS) < elapsed {
        Some(elapsed.as_secs_f64().ceil() as u64)
    } else {
        None
    }
}

fn delegate_result_value(
    text: String,
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> CborValue {
    if duration_seconds.is_none() && self_agent_id.is_none() && agent_id.is_none() {
        return CborValue::Text(text);
    }
    CborValue::Map(delegate_detail_entries(
        Some(text),
        duration_seconds,
        self_agent_id,
        agent_id,
    ))
}

fn delegate_error_details(
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> Option<CborValue> {
    if duration_seconds.is_none() && self_agent_id.is_none() && agent_id.is_none() {
        return None;
    }
    Some(CborValue::Map(delegate_detail_entries(
        None,
        duration_seconds,
        self_agent_id,
        agent_id,
    )))
}

fn delegate_detail_entries(
    output: Option<String>,
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> Vec<(CborValue, CborValue)> {
    let mut entries = Vec::new();
    if let Some(self_agent_id) = self_agent_id {
        entries.push((
            CborValue::Text("self_agent_id".to_owned()),
            CborValue::Text(self_agent_id.to_owned()),
        ));
    }
    if let Some(agent_id) = agent_id {
        entries.push((
            CborValue::Text("sub_agent_id".to_owned()),
            CborValue::Text(agent_id.to_owned()),
        ));
    }
    if let Some(duration_seconds) = duration_seconds {
        entries.push((
            CborValue::Text("duration_seconds".to_owned()),
            CborValue::Integer((duration_seconds as i64).into()),
        ));
    }
    if let Some(output) = output {
        entries.push((
            CborValue::Text("output".to_owned()),
            CborValue::Text(output),
        ));
    }
    entries
}

fn parse_cancel_args(arguments: &CborValue) -> Result<ToolCallId, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        if name == "tool_call_id" {
            return match v {
                CborValue::Text(text) if !text.is_empty() => Ok(text.clone().into()),
                CborValue::Text(_) => Err("`tool_call_id` must not be empty".to_owned()),
                _ => Err("`tool_call_id` must be a string".to_owned()),
            };
        }
    }
    Err("`tool_call_id` is required".to_owned())
}

fn skill_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(SKILL_TOOL_NAME),
        model_visible_name: None,
        description: Some("Discover and load skills — short, focused playbooks for specific tasks. The user has likely curated skills for workflows they care about, so reach for this tool early: before tackling any request that touches a tool, command, framework, or domain you are not deeply familiar with — or anything the user might have an opinionated way of doing. Most skills are NOT pre-advertised in <available_skills>, so a missing entry there is no reason to skip this tool. Pass a query string; punctuation separates terms except hyphens inside skill names. If the search resolves to one skill, or a single-term query exactly matches a skill name, the full skill is loaded; otherwise matching skill names and descriptions are returned with guidance. Query terms are split on punctuation, lowercased, and deduplicated; hyphenated skill names are preserved. To load a specific ambiguous result, call this tool again with only the exact skill name.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Keywords matched case-insensitively against skill names and descriptions. Punctuation separates terms except hyphens inside skill names; terms are lowercased and deduplicated. Use only an exact skill name to load a specific ambiguous result."},"search_content":{"type":"boolean","description":"When true, also search the first 64 KiB of the skill file after stripping frontmatter from that prefix. Default false."}},"required":["query"],"additionalProperties":false})),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    }
}

fn delegate_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(DELEGATE_TOOL_NAME), model_visible_name: None, description: Some("Delegate a self-contained sub-task to a fresh sub-agent that runs with its own context and tools, and returns only its final text answer. The instant background placeholder and final result include `self_agent_id` and `sub_agent_id` headers/values. Pass `sub_agent_id` to `message`.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"task_name":{"type":"string","description":"Short human-readable label for the sub-task (a few words, lowercase). Surfaced live to the user as `delegate [task_name]` while the sub-agent runs."},"prompt":{"type":"string","description":"Self-contained task for the sub-agent."},"execution_mode":{"type":"string","enum":["shared","update","exclusive"],"description":"Default: `shared`."},"role":{"type":"string","description":"Optional sub-agent role to use."}},"required":["task_name","prompt"],"additionalProperties":false})), format: None, enabled_by_default: true, execution_mode: ToolExecutionMode::Shared, background_support: Some(BackgroundSupport::Instant) }
}

fn message_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(MESSAGE_TOOL_NAME), model_visible_name: None, description: Some("Send an async message to another live or pending agent, or to the user. Use recipient_id `user`, or a `sub_agent_id` returned by `delegate`; UI display depends on `/set show-messages`. A non-user recipient also receives a hidden prompt. Requires `recipient_id` and `message`.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"recipient_id":{"type":"string","description":"Recipient agent_id, or the special value `user`."},"message":{"type":"string","description":"Message body."}},"required":["recipient_id","message"],"additionalProperties":false})), format: None, enabled_by_default: true, execution_mode: ToolExecutionMode::Shared, background_support: Some(BackgroundSupport::Never) }
}

fn cancel_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(CANCEL_TOOL_NAME), model_visible_name: None, description: Some("Cancel a running supported background tool call. Requires `tool_call_id`; currently delegate and shell tool calls can be canceled. Duplicate cancellation requests for the same tool call fail when tracked.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"tool_call_id":{"type":"string","description":"Required id of the running supported background tool call to cancel."}},"required":["tool_call_id"],"additionalProperties":false})), format: None, enabled_by_default: true, execution_mode: ToolExecutionMode::Shared, background_support: Some(BackgroundSupport::Never) }
}

fn wait_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(WAIT_TOOL_NAME), model_visible_name: None, description: Some("Wait for background tool calls. With `tool_call_id`, wait for that specific background call. Without `tool_call_id`, wait for the first background call in this conversation to finish and return its `original_tool_call_id`. Already-finished matching results return immediately. Tau will notify you via marked internal messages about background calls completing; `wait({})` consumes one completion and suppresses that completion notice.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"tool_call_id":{"type":"string","description":"Optional. When set, wait for this specific background tool call. When omitted, wait for the first background tool call in this conversation to finish."}},"additionalProperties":false})), format: None, enabled_by_default: true, execution_mode: ToolExecutionMode::Shared, background_support: Some(BackgroundSupport::Never) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delegate_args_with_mode(mode: &str) -> CborValue {
        CborValue::Map(vec![
            (
                CborValue::Text("task_name".to_owned()),
                CborValue::Text("task".to_owned()),
            ),
            (
                CborValue::Text("prompt".to_owned()),
                CborValue::Text("do the task".to_owned()),
            ),
            (
                CborValue::Text("execution_mode".to_owned()),
                CborValue::Text(mode.to_owned()),
            ),
        ])
    }

    fn cbor_map_text<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
        let CborValue::Map(entries) = value else {
            return None;
        };
        entries.iter().find_map(|(entry_key, entry_value)| {
            matches!(entry_key, CborValue::Text(text) if text == key)
                .then_some(entry_value)
                .and_then(|value| match value {
                    CborValue::Text(text) => Some(text.as_str()),
                    _ => None,
                })
        })
    }

    #[test]
    fn delegate_tool_schema_advertises_update_execution_mode() {
        let spec = delegate_tool_spec();
        let parameters = spec.parameters.expect("parameters");
        assert_eq!(
            parameters["properties"]["execution_mode"]["enum"],
            serde_json::json!(["shared", "update", "exclusive"])
        );
    }

    #[test]
    fn delegate_result_includes_only_caller_and_sub_agent_ids() {
        let value = delegate_result_value(
            "done".to_owned(),
            None,
            Some("engineer_parent"),
            Some("engineer_child"),
        );

        assert_eq!(
            cbor_map_text(&value, "self_agent_id"),
            Some("engineer_parent")
        );
        assert_eq!(
            cbor_map_text(&value, "sub_agent_id"),
            Some("engineer_child")
        );
        assert_eq!(cbor_map_text(&value, "agent_id"), None);
        assert_eq!(cbor_map_text(&value, "output"), Some("done"));
    }

    /// Delegate accepts the `update` mode advertised in the tool schema and
    /// forwards it to the global sub-agent scheduler.
    #[test]
    fn delegate_args_accept_update_execution_mode() {
        let parsed = parse_delegate_args(&delegate_args_with_mode("update")).expect("parse");
        assert_eq!(parsed.execution_mode, ToolExecutionMode::Update);
    }

    /// Bad mode diagnostics list every accepted spelling so model-visible tool
    /// errors are actionable.
    #[test]
    fn delegate_args_execution_mode_error_mentions_update() {
        let error = parse_delegate_args(&delegate_args_with_mode("mutating")).expect_err("error");
        assert_eq!(
            error,
            "`execution_mode` must be `shared`, `update`, or `exclusive`"
        );
    }
}
