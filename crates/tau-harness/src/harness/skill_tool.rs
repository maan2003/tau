//! The harness-owned `skill` tool.
//!
//! The `skill` tool is registered against [`HARNESS_CONNECTION_ID`] in
//! [`Harness::register_harness_tools`] and dispatched inline (bypassing the
//! bus) by [`Harness::handle_skill_tool_call`]. It surfaces the skills the
//! harness already discovered at startup (`Harness::discovered_skills`),
//! so search and load don't touch the filesystem walker again.

use std::io::Read;

use tau_proto::{
    AgentToolCall, CborValue, Event, ToolCallId, ToolDisplay, ToolDisplayStats, ToolDisplayStatus,
    ToolName, ToolRequest,
};

const MAX_SKILL_CONTENT_BYTES: usize = 64 * 1024;
const MAX_SKILL_SEARCH_MATCHES: usize = 50;

use crate::conversation::ConversationId;
use crate::error::HarnessError;
use crate::harness::{HARNESS_CONNECTION_ID, Harness};

impl Harness {
    /// Register harness-owned tools (e.g. `skill`).
    pub(crate) fn register_harness_tools(&mut self) {
        let _ = self.registry.register(
            HARNESS_CONNECTION_ID,
            tau_proto::ToolSpec {
                name: ToolName::new("skill"),
                model_visible_name: None,
                description: Some(
                    "Discover and load skills — short, focused playbooks for \
                     specific tasks. The user has likely curated skills for \
                     workflows they care about, so reach for this tool early: \
                     before tackling any request that touches a tool, command, \
                     framework, or domain you are not deeply familiar with — or \
                     anything the user might have an opinionated way of doing. \
                     Most skills are NOT pre-advertised in <available_skills>, so \
                     a missing entry there is no reason to skip this tool. Pass \
                     a query string; punctuation separates terms except hyphens \
                     inside skill names. If the search \
                     resolves to one skill, or a single-term query exactly \
                     matches a skill name, the full skill is loaded; otherwise \
                     matching skill names and descriptions are returned with \
                     guidance. Query terms are split on punctuation, \
                     lowercased, and deduplicated; hyphenated skill names are \
                     preserved. To load a specific ambiguous result, call this \
                     tool again with only the exact skill name."
                        .to_owned(),
                ),
                tool_type: tau_proto::ToolType::Function,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keywords matched case-insensitively against skill names and descriptions. Punctuation separates terms except hyphens inside skill names; terms are lowercased and deduplicated. Use only an exact skill name to load a specific ambiguous result."
                        },
                        "search_content": {
                            "type": "boolean",
                            "description": "When true, also search the first 64 KiB of the skill file after stripping frontmatter from that prefix. Default false."
                        }
                    },
                    "required": ["query"]
                })),
                format: None,
                enabled_by_default: true,
                side_effects: tau_proto::ToolSideEffects::Pure,
            },
        );
    }

    /// Handle the harness-owned `skill` tool call inline.
    ///
    /// Searches by `query`, then auto-loads when the result is unambiguous:
    /// - one total match loads that skill;
    /// - one single-term query with an exact skill-name match loads that skill;
    /// - otherwise returns `{name, description}` matches.
    pub(crate) fn handle_skill_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        let tool_name = ToolName::new("skill");

        // Track the conversation mapping first so the published
        // request + result both attribute to this conversation's
        // session via `session_id_for_event`.
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tool_names
            .insert(call_id.clone(), tool_name.clone());
        self.bump_tools_started_for(cid);
        self.publish_for_conversation(
            cid,
            Event::ToolRequest(ToolRequest {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                originator: tau_proto::PromptOriginator::User,
            }),
        );

        let result_event = self.handle_skill_query(&call_id, &tool_name, &call.arguments);

        // Publish, then drop the in-flight tracking — order matters:
        // `session_id_for_event` reads `tool_conversations` to
        // attribute the persisted record before we clear it.
        self.publish_for_conversation(cid, result_event);
        self.on_tool_call_complete(&call.id);
        self.clear_tool_call_tracking(call_id.as_str());

        Ok(())
    }

    fn read_skill_by_name(
        &mut self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        name: &str,
    ) -> Event {
        let Some(skill) = self.discovered_skills.get(name) else {
            let message = format!("unknown skill: {name}");
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                display: Some(skill_error_display(name, &message)),
                message,
                details: None,
                originator: tau_proto::PromptOriginator::User,
            });
        };
        let file_path = skill.file_path.clone();
        let description = skill.description.clone();
        match read_text_file_prefix(&file_path, MAX_SKILL_CONTENT_BYTES) {
            Ok(read) => {
                let mut body = match skill_body_from_prefix(&read) {
                    Ok(body) => body,
                    Err(message) => {
                        return Event::ToolError(tau_proto::ToolError {
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                            display: Some(skill_error_display(name, &message)),
                            message,
                            details: None,
                            originator: tau_proto::PromptOriginator::User,
                        });
                    }
                };
                if read.truncated {
                    self.emit_info_important(&format!(
                        "skill too long: {} truncated to {MAX_SKILL_CONTENT_BYTES} bytes while loading {}",
                        file_path.display(),
                        name,
                    ));
                    body.push_str(&format!(
                        "\n\n[skill content truncated at {MAX_SKILL_CONTENT_BYTES} bytes; file has {} bytes]",
                        read.total_bytes
                    ));
                }
                let mut display = skill_ok_display(name);
                display.stats = text_stats_for_skill(&body);
                Event::ToolResult(tau_proto::ToolResult {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    result: CborValue::Map(vec![
                        (
                            CborValue::Text("name".to_owned()),
                            CborValue::Text(name.to_owned()),
                        ),
                        (
                            CborValue::Text("description".to_owned()),
                            CborValue::Text(description),
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
                    display: Some(display),
                    originator: tau_proto::PromptOriginator::User,
                })
            }
            Err(e) => {
                let message = format!("failed to read skill file: {e}");
                Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    display: Some(skill_error_display(name, &message)),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                })
            }
        }
    }

    fn handle_skill_query(
        &mut self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        arguments: &CborValue,
    ) -> Event {
        let needles = match extract_skill_search_queries(arguments) {
            Ok(needles) => needles,
            Err(message) => {
                return Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    display: Some(skill_error_display("search:", &message)),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                });
            }
        };
        let search_content = match extract_optional_bool(arguments, "search_content") {
            Ok(value) => value.unwrap_or(false),
            Err(message) => {
                return Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    display: Some(skill_error_display("search:", &message)),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                });
            }
        };
        let outcome = self.search_discovered_skills(&needles, search_content);
        for warning in &outcome.warnings {
            self.emit_info_important(warning);
        }

        if let Some(name) = outcome.auto_load_name.clone() {
            return self.read_skill_by_name(call_id, tool_name, &name);
        }

        self.skill_search_result(call_id, tool_name, &needles, search_content, outcome)
    }

    fn skill_search_result(
        &self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        needles: &[String],
        search_content: bool,
        outcome: SkillSearchOutcome,
    ) -> Event {
        let scope_label = if search_content { " [content]" } else { "" };
        let queries_label = needles.join(" ");
        let display_args = format!("{queries_label}{scope_label}");

        let mut display = skill_ok_display(&display_args);
        display.stats = skill_search_stats(&outcome.hits);
        if outcome.total_matches == 0 {
            display.status_text = "ok: no matches".to_owned();
        } else if outcome.truncated {
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
        let queries_echo =
            CborValue::Array(needles.iter().map(|n| CborValue::Text(n.clone())).collect());
        let guidance = skill_search_guidance(total_matches);
        Event::ToolResult(tau_proto::ToolResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            result: CborValue::Map(vec![
                (CborValue::Text("queries".to_owned()), queries_echo),
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
                    CborValue::Text(guidance),
                ),
            ]),
            display: Some(display),
            originator: tau_proto::PromptOriginator::User,
        })
    }

    /// Score each discovered skill by how many of `needles` match its
    /// name, description, and (when `search_content`) body. A skill
    /// that matches more terms is more likely the right answer when
    /// the agent fired several plausible spellings at the same time
    /// ("commit", "git commit", "version control"). Returns
    /// rows sorted by descending matched term count, with ties broken
    /// by name for deterministic output.
    ///
    /// Needles are expected to already be lowercased.
    fn search_discovered_skills(
        &self,
        needles: &[String],
        search_content: bool,
    ) -> SkillSearchOutcome {
        let mut warnings = Vec::new();
        let mut hits = Vec::new();
        let mut total_matches = 0;
        let mut only_hit_name = None;
        let mut exact_hit_name = None;

        for (name, skill) in &self.discovered_skills {
            let lower_name = name.as_str().to_lowercase();
            let lower_desc = skill.description.to_lowercase();
            // Read the body at most once across all needles, and
            // only when the caller opted in.
            let mut body: Option<String> = None;
            let mut matched_fields = Vec::new();
            let mut matched_terms = 0;
            for needle in needles {
                let mut matched = false;
                if lower_name.contains(needle.as_str()) {
                    matched = true;
                    push_matched_field(&mut matched_fields, "name");
                }
                if lower_desc.contains(needle.as_str()) {
                    matched = true;
                    push_matched_field(&mut matched_fields, "description");
                }
                if search_content {
                    let body = body.get_or_insert_with(|| {
                        match read_text_file_prefix(&skill.file_path, MAX_SKILL_CONTENT_BYTES) {
                            Ok(read) => match skill_body_from_prefix(&read) {
                                Ok(body) => {
                                    if read.truncated {
                                        let warning = format!(
                                            "skill too long: {} truncated to {MAX_SKILL_CONTENT_BYTES} bytes while content-searching {}",
                                            skill.file_path.display(),
                                            name.as_str(),
                                        );
                                        tracing::warn!(%warning);
                                        warnings.push(warning);
                                    }
                                    body.to_lowercase()
                                }
                                Err(message) => {
                                    let warning = format!(
                                        "skill frontmatter too long: {} while content-searching {}: {message}",
                                        skill.file_path.display(),
                                        name.as_str(),
                                    );
                                    tracing::warn!(%warning);
                                    warnings.push(warning);
                                    String::new()
                                }
                            },
                            Err(err) => {
                                tracing::warn!(
                                    skill = %name.as_str(),
                                    path = %skill.file_path.display(),
                                    error = %err,
                                    "skill body unreadable; treating as empty for content search",
                                );
                                String::new()
                            }
                        }
                    });
                    if body.contains(needle.as_str()) {
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
                Some(name.as_str().to_owned())
            } else {
                None
            };
            if needles.len() == 1 && name.as_str() == needles[0] {
                exact_hit_name = Some(name.as_str().to_owned());
            }

            hits.push(SkillSearchHit {
                matched_terms,
                matched_fields,
                name: name.as_str().to_owned(),
                description: tau_skills::truncate_description(&skill.description).into_owned(),
            });
            sort_skill_hits(&mut hits);
            if MAX_SKILL_SEARCH_MATCHES < hits.len() {
                hits.truncate(MAX_SKILL_SEARCH_MATCHES);
            }
        }

        let auto_load_name = if total_matches == 1 {
            only_hit_name
        } else {
            exact_hit_name
        };
        let truncated = MAX_SKILL_SEARCH_MATCHES < total_matches;

        SkillSearchOutcome {
            hits,
            total_matches,
            truncated,
            auto_load_name,
            warnings,
        }
    }
}

struct SkillSearchHit {
    matched_terms: usize,
    matched_fields: Vec<String>,
    name: String,
    description: String,
}

fn sort_skill_hits(hits: &mut [SkillSearchHit]) {
    hits.sort_by(|a, b| {
        b.matched_terms
            .cmp(&a.matched_terms)
            .then_with(|| a.name.cmp(&b.name))
    });
}

struct SkillSearchOutcome {
    hits: Vec<SkillSearchHit>,
    total_matches: usize,
    truncated: bool,
    auto_load_name: Option<String>,
    warnings: Vec<String>,
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
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(LimitedTextRead {
        text,
        truncated,
        total_bytes,
    })
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
    let chip = error_chip_text(message);
    ToolDisplay {
        args: args.to_owned(),
        status: ToolDisplayStatus::Error,
        status_text: chip,
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
        return "No skills matched. Try different terms, fewer terms, or set search_content: true if the body may mention the topic."
            .to_owned();
    }
    "Call `skill` again with only an exact `name` to load a specific match, or narrow `query` with a more distinctive term. Multi-term queries use OR semantics and rank by matched term count, so adding generic terms may not reduce matches. The top match was not auto-loaded because other matches also existed."
        .to_owned()
}

fn push_matched_field(fields: &mut Vec<String>, field: &str) {
    if !fields.iter().any(|existing| existing == field) {
        fields.push(field.to_owned());
    }
}

fn error_chip_text(message: &str) -> String {
    let first = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        return "err".to_owned();
    }
    const MAX: usize = 64;
    let label = if first.chars().count() <= MAX {
        first.to_owned()
    } else {
        let mut s: String = first.chars().take(MAX.saturating_sub(1)).collect();
        s.push('…');
        s
    };
    format!("err: {label}")
}

/// Parse the `query` argument of a `skill` tool call into one-or-more
/// lowercased search needles. The query is a single string;
/// punctuation separates terms except for hyphens inside skill names.
/// Terms are lowercased and deduplicated before matching. Returns a
/// user-facing error message string on missing/empty/malformed input.
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

pub(super) fn normalized_skill_query_terms(raw_query: &str) -> Vec<String> {
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
