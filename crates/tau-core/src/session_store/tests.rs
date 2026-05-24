use super::*;

/// Internal steering text is agent-visible context, but it must not replace
/// the latest human prompt preview shown in session metadata.
#[test]
fn latest_user_prompt_text_ignores_internal_steered_prompts() {
    let user_event = Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
        session_id: "s1".into(),
        text: "visible user prompt".to_owned(),
        target_agent_id: None,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    assert_eq!(user_prompt_text(&user_event), Some("visible user prompt"));

    let internal_event = Event::SessionPromptSteered(tau_proto::SessionPromptSteered {
        session_id: "s1".into(),
        text: "[tau-internal] Tool call `bg` is complete.".to_owned(),
        message_class: tau_proto::PromptMessageClass::Internal,
    });
    assert_eq!(user_prompt_text(&internal_event), None);
}
