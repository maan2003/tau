use super::{AgentActivity, MessageRenderMode, RoleCompletionDetails, role_value_completion};

fn agent_message(sender_id: &str, recipient_id: &str, message: &str) -> tau_proto::AgentMessage {
    tau_proto::AgentMessage {
        session_id: "session".into(),
        sender_id: sender_id.to_owned(),
        recipient_id: recipient_id.to_owned(),
        message: message.to_owned(),
    }
}

/// `/set show-messages` must hide, summarize, or fully render durable
/// message events based on whether they involve the user. This locks the
/// privacy modes without needing a terminal renderer fixture.
#[test]
fn show_messages_modes_map_user_and_agent_messages() {
    let user_sender_message = agent_message("user", "agent", "visible body");
    let user_recipient_message = agent_message("agent", "user", "visible body");
    let agent_message = agent_message("agent-a", "agent-b", "private body");

    let cases = [
        (
            tau_config::settings::ShowMessages::None,
            MessageRenderMode::Hidden,
            MessageRenderMode::Hidden,
        ),
        (
            tau_config::settings::ShowMessages::SelfSummary,
            MessageRenderMode::Summary,
            MessageRenderMode::Hidden,
        ),
        (
            tau_config::settings::ShowMessages::SelfFull,
            MessageRenderMode::Full,
            MessageRenderMode::Hidden,
        ),
        (
            tau_config::settings::ShowMessages::AllSummary,
            MessageRenderMode::Full,
            MessageRenderMode::Summary,
        ),
        (
            tau_config::settings::ShowMessages::AllFull,
            MessageRenderMode::Full,
            MessageRenderMode::Full,
        ),
    ];

    for (mode, expected_self, expected_agent) in cases {
        assert_eq!(
            super::EventRenderer::message_render_mode(mode, &user_sender_message),
            expected_self
        );
        assert_eq!(
            super::EventRenderer::message_render_mode(mode, &user_recipient_message),
            expected_self
        );
        assert_eq!(
            super::EventRenderer::message_render_mode(mode, &agent_message),
            expected_agent
        );
    }
}

/// Summary rendering intentionally carries no message body so private
/// content from summarized agent-agent messages cannot leak.
#[test]
fn agent_message_summary_excludes_body() {
    let message = agent_message("agent-a", "agent-b", "secret payload");

    let summary = super::EventRenderer::agent_message_summary(&message);

    assert_eq!(summary, "Message from agent-a to agent-b");
    assert!(!summary.contains("secret payload"));
}

fn tool_call(call_id: &str) -> tau_proto::ContextItem {
    tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
        call_id: call_id.into(),
        name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Null,
    })
}

/// Ctrl-D must stay guarded across the assistant/tool boundary: a
/// provider response that requests tools means the session is still
/// busy even though the provider turn itself has finished.
#[test]
fn agent_activity_stays_busy_until_requested_tools_finish() {
    let mut activity = AgentActivity::default();
    activity.mark_optimistic_submission();
    assert!(activity.is_in_progress());

    activity.start_prompt(&"sp1".into());
    activity.finish_prompt(&"sp1".into(), &[tool_call("call1")]);
    assert!(activity.is_in_progress());

    activity.finish_tool(&"call1".into());
    assert!(!activity.is_in_progress());
}

/// Side conversations use the same lifecycle events as the main chat;
/// the Ctrl-D guard must track them before UI filtering hides their
/// transcript details.
#[test]
fn agent_activity_tracks_side_conversation_prompts() {
    let mut activity = AgentActivity::default();
    activity.start_prompt(&"side-sp1".into());
    assert!(activity.is_in_progress());

    activity.finish_prompt(&"side-sp1".into(), &[]);
    assert!(!activity.is_in_progress());
}

#[test]
fn role_details_abbreviate_description() {
    let details = RoleCompletionDetails::from_description(
        "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off, tools=read_only, enable-tools=web_search",
    );

    assert_eq!(
        details.short_description(),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off tools=read_only et=web_search"
    );
}

/// `/role <name>` completion appends free-form role descriptions after the
/// parsed model/knob summary instead of parsing that user text as settings.
#[test]
fn role_details_append_configured_role_description() {
    let details = RoleCompletionDetails::from_role_info(&tau_proto::HarnessRoleInfo {
        name: "deep".to_owned(),
        description:
            "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off"
                .to_owned(),
        role_description: Some("Investigate deeply, no rush = thorough".to_owned()),
    });

    assert_eq!(
        details.short_description(),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off — Investigate deeply, no rush = thorough"
    );
}

#[test]
fn role_details_report_single_current_field() {
    let details = RoleCompletionDetails::from_description(
        "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off, service-tier=fast, tools=read_only, enable-tools=web_search",
    );

    assert_eq!(details.current_description("model"), "codex-dpcpw/gpt-5.5");
    assert_eq!(details.current_description("effort"), "xhigh");
    assert_eq!(details.current_description("verbosity"), "medium");
    assert_eq!(details.current_description("thinking-summary"), "off");
    assert_eq!(details.current_description("service-tier"), "fast");
    assert_eq!(details.current_description("tools"), "read_only");
    assert_eq!(details.current_description("enable-tools"), "web_search");
}

#[test]
fn role_values_have_descriptions() {
    let item = role_value_completion("thinking-summary", "detailed");

    assert_eq!(item.value, "detailed");
    assert_eq!(item.description, "detailed thinking summaries");
}
