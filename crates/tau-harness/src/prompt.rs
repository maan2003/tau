//! Building blocks for the per-turn prompt: the system prompt body, the
//! AGENTS.md context message, and the conversation assembly that turns a
//! [`tau_core::SessionTree`] into item-based prompt context.

use tau_core::SessionEntry;
use tau_proto::{CborValue, ContextItem, PromptFragment};

use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};

pub(crate) const BUILT_IN_SYSTEM_TEMPLATE_NAME: &str = "built-in";
const BUILT_IN_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/system.hbs");
const BIG_SYSTEM_TEMPLATE_NAME: &str = "big";
const BIG_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/big.hbs");

pub(crate) fn built_in_system_prompt_templates() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([
        (
            BUILT_IN_SYSTEM_TEMPLATE_NAME.to_owned(),
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE.to_owned(),
        ),
        (
            BIG_SYSTEM_TEMPLATE_NAME.to_owned(),
            BIG_SYSTEM_PROMPT_TEMPLATE.to_owned(),
        ),
    ])
}

/// Context made available to role prompt Handlebars templates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RolePromptTemplateContext<'a> {
    /// Name of the role whose prompt is being rendered.
    pub(crate) role_name: &'a str,
}

/// Builds the system prompt from Tau defaults plus role prompt and prompt
/// fragments.
///
/// Must be deterministic and stable across turns of the same session
/// — see the linear-prefix invariant in `send_prompt_to_agent`.
/// Tools and skills are sorted by name (HashMap iteration would
/// otherwise drift). The current date is intentionally omitted:
/// including it would invalidate the prompt cache every midnight
/// UTC.
#[cfg(test)]
pub(crate) fn build_system_prompt(
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
) -> String {
    build_system_prompt_with_template_context(
        BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
        skills,
        prompt_fragments,
        serde_json::json!({}),
        RolePromptTemplateContext { role_name: "" },
    )
}

/// Builds the system prompt with role prompt sections rendered as Handlebars.
pub(crate) fn build_system_prompt_with_template_context(
    system_template: &str,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    session_context: serde_json::Value,
    template_context: RolePromptTemplateContext<'_>,
) -> String {
    // Tool definitions are delivered out-of-band via the provider's
    // tool-use channel, so the built-in system template doesn't restate them.
    let fragments: Vec<_> = prompt_fragments.to_vec();
    render_system_prompt_template(
        system_template,
        template_context,
        skills,
        &fragments,
        session_context,
    )
}

fn render_system_prompt_template(
    system_template: &str,
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    session_context: serde_json::Value,
) -> String {
    let data = system_prompt_template_data(context, skills, prompt_fragments, session_context);
    let handlebars = prompt_template_renderer();
    match handlebars.render_template(system_template, &data) {
        Ok(rendered) => rendered,
        Err(error) => {
            tracing::warn!(
                role = context.role_name,
                error = %error,
                "failed to render system prompt handlebars template"
            );
            match handlebars.render_template(BUILT_IN_SYSTEM_PROMPT_TEMPLATE, &data) {
                Ok(rendered) => rendered,
                Err(error) => {
                    tracing::warn!(
                        role = context.role_name,
                        error = %error,
                        "failed to render built-in system prompt handlebars template; using unrendered template"
                    );
                    BUILT_IN_SYSTEM_PROMPT_TEMPLATE.to_owned()
                }
            }
        }
    }
}

fn prompt_template_data(
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    session_context: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "role": {
            "name": context.role_name,
        },
        "skills": prompt_template_skills(skills),
        "session_context": session_context,
    })
}

fn system_prompt_template_data(
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    session_context: serde_json::Value,
) -> serde_json::Value {
    let mut data = prompt_template_data(context, skills, session_context);
    let rendered_fragments = rendered_prompt_fragment_template_parts(prompt_fragments, &data);
    data.as_object_mut()
        .expect("system prompt template data is an object")
        .insert("prompt_fragments".to_owned(), rendered_fragments);
    data
}

fn rendered_prompt_fragment_template_parts(
    fragments: &[PromptFragment],
    data: &serde_json::Value,
) -> serde_json::Value {
    let handlebars = prompt_template_renderer();
    serde_json::Value::Array(
        {
            let mut ordered = fragments.iter().collect::<Vec<_>>();
            // Preserve the caller's deterministic source/name tie-break within
            // a priority bucket. The harness gathers tool fragments in
            // priority/source/name order before rendering.
            ordered.sort_by_key(|a| a.priority);
            ordered
        }
        .into_iter()
        .filter_map(|fragment| {
            if fragment.template.is_empty() {
                return None;
            }
            let content = match handlebars.render_template(fragment.template.as_str(), data) {
                Ok(rendered) => rendered,
                Err(error) => {
                    tracing::warn!(
                        fragment_name = fragment.name,
                        priority = fragment.priority.get(),
                        error = %error,
                        "failed to render prompt fragment template; skipping fragment"
                    );
                    return None;
                }
            };
            Some(serde_json::json!({
                "name": fragment.name,
                "priority": fragment.priority.get(),
                "content": content,
            }))
        })
        .collect(),
    )
}

fn prompt_template_renderer() -> handlebars::Handlebars<'static> {
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper("sort", Box::new(SortHelper));
    handlebars.register_helper("xml_escape", Box::new(XmlEscapeHelper));
    handlebars
}

fn prompt_template_skills(
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
) -> Vec<serde_json::Value> {
    let mut skills: Vec<_> = skills
        .iter()
        .filter(|(_, skill)| skill.add_to_prompt)
        .map(|(name, skill)| {
            let base_dir = match &skill.source {
                crate::discovery::DiscoveredSkillSource::File(path) => path
                    .parent()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                crate::discovery::DiscoveredSkillSource::BuiltIn { .. } => "<builtin>".to_owned(),
            };
            serde_json::json!({
                "name": name.as_str(),
                "description": tau_skills::truncate_description(&skill.description),
                "baseDir": base_dir,
            })
        })
        .collect();
    skills.sort_by(|a, b| compare_template_values(a, b, Some("name")));
    skills
}

struct XmlEscapeHelper;

impl handlebars::HelperDef for XmlEscapeHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        use handlebars::JsonRender;

        let Some(value) = h.param(0) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
                String::new(),
            )));
        };
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
            xml_escape(&value.value().render()),
        )))
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

struct SortHelper;

impl handlebars::HelperDef for SortHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        let Some(values) = h.param(0).and_then(|param| param.value().as_array()) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::Array(
                Vec::new(),
            )));
        };
        let key = h.hash_get("by").and_then(|param| param.value().as_str());
        let mut sorted = values.clone();
        sorted.sort_by(|a, b| compare_template_values(a, b, key));
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::Array(
            sorted,
        )))
    }
}

fn compare_template_values(
    a: &serde_json::Value,
    b: &serde_json::Value,
    key: Option<&str>,
) -> std::cmp::Ordering {
    let a = key.and_then(|key| a.get(key)).unwrap_or(a);
    let b = key.and_then(|key| b.get(key)).unwrap_or(b);
    match (a, b) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a.cmp(b),
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a.cmp(b),
        _ => value_type_rank(a)
            .cmp(&value_type_rank(b))
            .then_with(|| a.to_string().cmp(&b.to_string())),
    }
}

fn value_type_rank(value: &serde_json::Value) -> u8 {
    match value {
        serde_json::Value::Null => 0,
        serde_json::Value::Bool(_) => 1,
        serde_json::Value::Number(_) => 2,
        serde_json::Value::String(_) => 3,
        serde_json::Value::Array(_) => 4,
        serde_json::Value::Object(_) => 5,
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
                if key == "output" {
                    parts.push(value);
                } else if value.contains('\n') || key == "line-numbered content" {
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

    fn discovered_skill(description: &str, add_to_prompt: bool) -> DiscoveredSkill {
        DiscoveredSkill {
            source_id: "test-extension".into(),
            description: description.to_owned(),
            source: crate::discovery::DiscoveredSkillSource::BuiltIn {
                content: std::borrow::Cow::Borrowed(""),
            },
            add_to_prompt,
        }
    }

    fn cwd_prompt_fragment() -> tau_proto::PromptFragment {
        tau_proto::PromptFragment::new(
            "shell.cwd",
            tau_proto::PromptPriority::new(10),
            "{{#each session_context.cwd}}{{#if @first}}Current working directory: {{value}}{{/if}}{{/each}}",
        )
    }

    #[test]
    fn build_system_prompt_without_fragments_does_not_render_cwd_prose() {
        let skills = std::collections::HashMap::new();
        let prompt = build_system_prompt(&skills, &[]);
        assert!(prompt.contains("expert coding assistant"));
        assert!(!prompt.contains("Current working directory: /tmp/work"));
    }

    /// Prompt templates are not HTML documents. Path-like context must render
    /// exactly so the model can pass it back to shell/file tools.
    #[test]
    fn build_system_prompt_does_not_html_escape_cwd() {
        let skills = std::collections::HashMap::new();
        let prompt = build_system_prompt_with_template_context(
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
            &skills,
            &[cwd_prompt_fragment()],
            serde_json::json!({
                "cwd": [
                    { "extension_name": "tau-ext-shell", "value": "/tmp/a&b<quoted>" }
                ]
            }),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
        );

        assert!(prompt.contains("Current working directory: /tmp/a&b<quoted>"));
        assert!(!prompt.contains("/tmp/a&amp;b&lt;quoted&gt;"));
    }

    #[test]
    fn build_system_prompt_encourages_parallel_tool_calls() {
        let skills = std::collections::HashMap::new();
        let prompt = build_system_prompt(&skills, &[]);
        assert!(prompt.contains("parallel"));
        assert!(prompt.contains("make all independent tool calls in parallel"));
    }

    /// Role prompts are configuration templates. They should be rendered just
    /// before insertion so prompts can refer to stable per-prompt context.
    #[test]
    fn build_system_prompt_renders_role_prompt_handlebars_context() {
        let skills = std::collections::HashMap::new();
        let fragments = vec![
            tau_proto::PromptFragment::new(
                "engineer.instructions",
                tau_proto::PromptPriority::new(100),
                "ROLE {{role.name}} is working in {{#each session_context.cwd}}{{#if @first}}{{value}}{{/if}}{{/each}}.",
            ),
            tau_proto::PromptFragment::new(
                "engineer.extra",
                tau_proto::PromptPriority::new(101),
                "EXTRA {{role.name}}",
            ),
        ];

        let prompt = build_system_prompt_with_template_context(
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
            &skills,
            &fragments,
            serde_json::json!({
                "cwd": [
                    { "extension_name": "tau-ext-shell", "value": "/tmp/work" }
                ]
            }),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
        );

        assert!(prompt.contains("ROLE engineer is working in /tmp/work"));
        assert!(prompt.contains("EXTRA engineer"));
        assert!(!prompt.contains("{{role.name}}"));
    }

    /// Templates receive the prompt-visible skills and can sort them
    /// explicitly so custom role prompts control their presentation.
    #[test]
    fn build_system_prompt_exposes_sortable_skills_to_handlebars() {
        let skills = std::collections::HashMap::from([
            (
                tau_proto::SkillName::from("zeta"),
                discovered_skill("last skill", true),
            ),
            (
                tau_proto::SkillName::from("alpha"),
                discovered_skill("first skill", true),
            ),
            (
                tau_proto::SkillName::from("hidden"),
                discovered_skill("hidden skill", false),
            ),
        ]);
        let fragments = vec![tau_proto::PromptFragment::new(
            "role.engineer.skills",
            tau_proto::PromptPriority::new(100),
            r#"Skills:
{{#each (sort skills by="name")}}* {{name}} - {{description}}
{{/each}}"#,
        )];

        let prompt = build_system_prompt_with_template_context(
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
            &skills,
            &fragments,
            serde_json::json!({}),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
        );

        let alpha = prompt.find("* alpha - first skill").expect("alpha skill");
        let zeta = prompt.find("* zeta - last skill").expect("zeta skill");
        assert!(alpha < zeta);
        assert!(!prompt.contains("hidden skill"));
    }

    /// The built-in skills section is XML-shaped, so it must escape only that
    /// section explicitly even though prompt templates otherwise render raw
    /// text for paths and user-authored role instructions.
    #[test]
    fn build_system_prompt_xml_escapes_builtin_skill_section() {
        let skills = std::collections::HashMap::from([(
            tau_proto::SkillName::from("a&b"),
            discovered_skill("use <fast> \"mode\"", true),
        )]);

        let prompt = build_system_prompt(&skills, &[]);

        assert!(prompt.contains("<name>a&amp;b</name>"));
        assert!(prompt.contains("<description>use &lt;fast&gt; &quot;mode&quot;</description>"));
    }

    /// Without a `by` hash, the sort helper sorts the items themselves rather
    /// than assuming object-shaped values with a `name` field.
    #[test]
    fn build_system_prompt_sort_helper_sorts_scalar_items_without_default_key() {
        let skills = std::collections::HashMap::new();
        let template = tau_proto::PromptContent::new(
            r#"{{#each (sort numbers)}}{{this}} {{/each}}
{{#each (sort words)}}{{this}} {{/each}}"#,
        );

        let prompt = build_system_prompt_with_template_context(
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
            &skills,
            &[],
            serde_json::json!({}),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
        );

        // Missing variables keep this role template from rendering in strict
        // mode, so exercise the helper directly with the shared renderer.
        let data = serde_json::json!({
            "numbers": [10, 2, 1],
            "words": ["zeta", "alpha", "middle"],
        });
        let handlebars = prompt_template_renderer();
        let rendered = handlebars
            .render_template(template.as_str(), &data)
            .expect("template renders");

        assert_eq!(
            rendered,
            "1 2 10 
alpha middle zeta "
        );
        assert!(!prompt.contains("Current working directory: /tmp/work"));
    }

    /// Session context is nested below `session_context`, so extension keys
    /// cannot collide with built-in prompt fields like `cwd` or `role`.
    #[test]
    fn build_system_prompt_exposes_session_context_to_handlebars() {
        let skills = std::collections::HashMap::new();
        let fragments = vec![tau_proto::PromptFragment::new(
            "role.engineer.context",
            tau_proto::PromptPriority::new(100),
            "{{#each session_context.skills}}{{extension_name}}={{value.count}}{{/each}}",
        )];

        let prompt = build_system_prompt_with_template_context(
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
            &skills,
            &fragments,
            serde_json::json!({
                "skills": [
                    { "extension_name": "core-skills", "value": { "count": 2 } }
                ]
            }),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
        );

        assert!(prompt.contains("core-skills=2"));
    }

    /// Prompt fragments are Handlebars templates rendered against the same
    /// prompt context as role templates, including extension-published session
    /// context.
    #[test]
    fn prompt_fragment_renders_session_context_variable() {
        let fragments = vec![tau_proto::PromptFragment::new(
            "tool.context",
            tau_proto::PromptPriority::new(10),
            "fragment={{#each session_context.demo}}{{extension_name}}:{{value.answer}}{{/each}}",
        )];

        let prompt = build_system_prompt_with_template_context(
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
            &std::collections::HashMap::new(),
            &fragments,
            serde_json::json!({
                "demo": [
                    { "extension_name": "demo-ext", "value": { "answer": 42 } }
                ]
            }),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
        );

        assert!(prompt.contains("fragment=demo-ext:42"));
    }

    /// Fragment ordering is deterministic and the rendered fragment data keeps
    /// priority visible for templates or debugging, not just for sorting.
    #[test]
    fn prompt_fragments_order_by_priority_name_and_expose_priority() {
        let fragments = vec![
            tau_proto::PromptFragment::new("a", tau_proto::PromptPriority::new(10), "A"),
            tau_proto::PromptFragment::new("c", tau_proto::PromptPriority::new(10), "C"),
            tau_proto::PromptFragment::new("b", tau_proto::PromptPriority::new(20), "B"),
        ];
        let data = system_prompt_template_data(
            RolePromptTemplateContext {
                role_name: "engineer",
            },
            &std::collections::HashMap::new(),
            &fragments,
            serde_json::json!({}),
        );
        let rendered = data["prompt_fragments"].as_array().expect("fragments");

        assert_eq!(rendered[0]["name"], serde_json::json!("a"));
        assert_eq!(rendered[0]["priority"], serde_json::json!(10));
        assert_eq!(rendered[1]["name"], serde_json::json!("c"));
        assert_eq!(rendered[2]["name"], serde_json::json!("b"));
    }

    /// The revived larger system prompt is shipped as a built-in template so
    /// roles can select it with `promptOverride: big` without copying it into
    /// user configuration.
    #[test]
    fn big_system_prompt_template_is_builtin_and_renders_context() {
        let templates = built_in_system_prompt_templates();
        assert!(templates.contains_key(BIG_SYSTEM_TEMPLATE_NAME));

        let skills = std::collections::HashMap::from([(
            tau_proto::SkillName::from("test-skill"),
            discovered_skill("test skill description", true),
        )]);
        let prompt = build_system_prompt_with_template_context(
            templates
                .get(BIG_SYSTEM_TEMPLATE_NAME)
                .expect("big prompt template exists"),
            &skills,
            &[tau_proto::PromptFragment::new(
                "test.fragment",
                tau_proto::PromptPriority::new(10),
                "FRAGMENT {{#each session_context.cwd}}{{#if @first}}{{value}}{{/if}}{{/each}}",
            )],
            serde_json::json!({
                "cwd": [
                    { "extension_name": "tau-ext-shell", "value": "/tmp/work" }
                ]
            }),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
        );

        assert!(prompt.contains("You are Tau, an autonomous coding agent."));
        assert!(prompt.contains("- test-skill: test skill description (file: <builtin>/SKILL.md)"));
        assert!(prompt.contains("FRAGMENT /tmp/work"));
    }

    /// Bad fragment templates are skipped rather than leaking raw unrendered
    /// Handlebars syntax into the model prompt.
    #[test]
    fn failed_prompt_fragment_is_skipped() {
        let fragments = vec![
            tau_proto::PromptFragment::new(
                "bad",
                tau_proto::PromptPriority::new(10),
                "BAD {{missing.value}}",
            ),
            tau_proto::PromptFragment::new("good", tau_proto::PromptPriority::new(20), "GOOD"),
        ];

        let prompt = build_system_prompt(&std::collections::HashMap::new(), &fragments);

        assert!(prompt.contains("GOOD"));
        assert!(!prompt.contains("BAD {{missing.value}}"));
    }

    /// Role prompt overrides, role extra prompts, and prompt fragments are
    /// distinct layers. This pins their composition order so adding more hook
    /// contributors later does not accidentally move tool instructions before
    /// role instructions or ignore hook priority ordering.
    #[test]
    fn build_system_prompt_composes_role_and_prompt_fragments_in_order() {
        let skills = std::collections::HashMap::new();
        let fragments = vec![
            cwd_prompt_fragment(),
            tau_proto::PromptFragment::new(
                "tool.early",
                tau_proto::PromptPriority::new(20),
                "TOOL EARLY",
            ),
            tau_proto::PromptFragment::new(
                "tool.late",
                tau_proto::PromptPriority::new(30),
                "TOOL LATE",
            ),
            tau_proto::PromptFragment::new(
                "engineer.instructions",
                tau_proto::PromptPriority::new(100),
                "ROLE PROMPT",
            ),
            tau_proto::PromptFragment::new(
                "engineer.extra",
                tau_proto::PromptPriority::new(101),
                "ROLE EXTRA",
            ),
        ];

        let prompt = build_system_prompt_with_template_context(
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
            &skills,
            &fragments,
            serde_json::json!({
                "cwd": [
                    { "extension_name": "tau-ext-shell", "value": "/tmp/work" }
                ]
            }),
            RolePromptTemplateContext {
                role_name: "engineer",
            },
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
        assert!(base < early);
        assert!(early < late);
        assert!(late < role);
        assert!(role < extra);
    }

    /// Empty hook entries are ignored without adding a blank prompt section.
    #[test]
    fn build_system_prompt_ignores_empty_prompt_fragment_sections() {
        let skills = std::collections::HashMap::new();
        let without_hook = build_system_prompt(&skills, &[]);
        let empty_fragments = vec![tau_proto::PromptFragment::new(
            "tool.empty",
            tau_proto::PromptPriority::new(10),
            "",
        )];
        let with_empty_hook = build_system_prompt(&skills, &empty_fragments);

        assert_eq!(with_empty_hook, without_hook);
    }

    #[test]
    fn cbor_to_text_puts_output_body_on_next_line_without_label() {
        let text = cbor_to_text(&CborValue::Map(vec![
            (
                CborValue::Text("status".to_owned()),
                CborValue::Integer(0.into()),
            ),
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text("1 only".to_owned()),
            ),
        ]));

        assert_eq!(text, "status: 0\n1 only");
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
        let detail_text = tool_result.output.render();

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
