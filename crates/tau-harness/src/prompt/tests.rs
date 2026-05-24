use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextRole, Event, MessageItem, ToolError,
    ToolResultStatus,
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
        tau_proto::PromptPriority::new(900),
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

#[test]
fn build_system_prompt_explains_tau_internal_marker() {
    let skills = std::collections::HashMap::new();
    let prompt = build_system_prompt(&skills, &[]);
    assert!(prompt.contains("[tau-internal]"));
    assert!(prompt.contains("background tool notifications"));
    assert!(prompt.contains("deduplicated tool result pointers"));
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
        &[],
        serde_json::json!({}),
    );
    let rendered = data["prompt_fragments"].as_array().expect("fragments");
    assert_eq!(rendered[0]["name"], serde_json::json!("a"));
    assert_eq!(rendered[0]["priority"], serde_json::json!(10));
    assert_eq!(rendered[0]["early"], serde_json::json!(true));
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

/// Tool-scoped fragments render in a dedicated section near tool-use
/// instructions, separate from ordinary role/extension prompt fragments.
#[test]
fn tool_prompt_fragments_render_in_dedicated_section() {
    let prompt = build_system_prompt_with_tool_template_context(
        BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
        &std::collections::HashMap::new(),
        &[tau_proto::PromptFragment::new(
            "role.instructions",
            tau_proto::PromptPriority::new(10),
            "ROLE FRAGMENT",
        )],
        &[tau_proto::PromptFragment::new(
            "tool.instructions",
            tau_proto::PromptPriority::new(10),
            "TOOL FRAGMENT",
        )],
        serde_json::json!({}),
        RolePromptTemplateContext {
            role_name: "engineer",
        },
    );

    let tool_heading = prompt
        .find("# Tool-provided instructions")
        .expect("tool section should render");
    let tool_fragment = prompt
        .find("TOOL FRAGMENT")
        .expect("tool fragment should render");
    let role_fragment = prompt
        .find("ROLE FRAGMENT")
        .expect("ordinary fragment should render");
    assert!(tool_heading < tool_fragment);
    assert!(tool_fragment < role_fragment);
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

/// Prompt priorities are split into coarse bands by the system template:
/// role/persona fragments below 100 render before generated context such as
/// skills, while higher-priority fragments render afterward. The cwd
/// fragment is intentionally late so it remains the prompt epilogue.
#[test]
fn build_system_prompt_composes_role_and_prompt_fragments_in_order() {
    let skills = std::collections::HashMap::from([(
        tau_proto::SkillName::from("test-skill"),
        discovered_skill("test skill", true),
    )]);
    let fragments = vec![
        tau_proto::PromptFragment::new(
            "manager.instructions",
            tau_proto::PromptPriority::new(5),
            "ROLE PROMPT",
        ),
        tau_proto::PromptFragment::new(
            "manager.extra",
            tau_proto::PromptPriority::new(6),
            "ROLE EXTRA",
        ),
        cwd_prompt_fragment(),
        tau_proto::PromptFragment::new(
            "tool.early",
            tau_proto::PromptPriority::new(120),
            "TOOL EARLY",
        ),
        tau_proto::PromptFragment::new(
            "tool.late",
            tau_proto::PromptPriority::new(130),
            "TOOL LATE",
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

    let skills = prompt
        .find("Skills provide specialized instructions")
        .expect("skills section should render");
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
    assert!(role < extra);
    assert!(extra < skills);
    assert!(skills < early);
    assert!(early < late);
    assert!(late < base);
    assert!(
        prompt
            .trim_end()
            .ends_with("Current working directory: /tmp/work")
    );
}

/// Prompt fragments can come from YAML block scalars and Handlebars
/// whitespace control. Normalize boundaries so fragments do not run
/// together and do not add trailing blank space to the system prompt.
#[test]
fn build_system_prompt_normalizes_prompt_fragment_spacing() {
    let skills = std::collections::HashMap::new();
    let prompt = build_system_prompt_with_template_context(
        BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
        &skills,
        &[
            tau_proto::PromptFragment::new(
                "role.manager.instructions",
                tau_proto::PromptPriority::new(5),
                "\nROLE PROMPT\n\n",
            ),
            tau_proto::PromptFragment::new(
                "shell.cwd",
                tau_proto::PromptPriority::new(900),
                "Current working directory: /tmp/work",
            ),
        ],
        serde_json::json!({}),
        RolePromptTemplateContext {
            role_name: "manager",
        },
    );

    assert!(prompt.contains("ROLE PROMPT\n\nCurrent working directory: /tmp/work"));
    assert!(!prompt.contains("ROLE PROMPTCurrent working directory"));
    assert!(prompt.ends_with('\n'));
    assert!(!prompt.ends_with("\n\n"));
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

    assert_eq!(text, "1 only");
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
        target_agent_id: None,
        message_class: tau_proto::PromptMessageClass::User,
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
    tree.apply_event(&Event::ProviderToolError(ToolError {
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
        target_agent_id: None,
        message_class: tau_proto::PromptMessageClass::User,
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
        target_agent_id: None,
        message_class: tau_proto::PromptMessageClass::User,
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
        target_agent_id: None,
        message_class: tau_proto::PromptMessageClass::User,
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
        target_agent_id: None,
        message_class: tau_proto::PromptMessageClass::User,
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
                ContextItem::Reasoning(serde_json::from_str(&blob).expect("opaque reasoning item")),
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
        target_agent_id: None,
        message_class: tau_proto::PromptMessageClass::User,
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
