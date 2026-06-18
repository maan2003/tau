//! Semantic persistence classification for harness events.

use tau_proto::Event;

/// Return whether an event should enter durable semantic stores.
pub(crate) fn should_persist(event: &Event) -> bool {
    !matches!(
        event,
        Event::ProviderResponseUpdated(_)
            | Event::ProviderPromptSubmitted(_)
            | Event::ToolProgress(_)
            | Event::ToolDelegateProgress(_)
            | Event::ActionSchemaPublished(_)
            | Event::ActionInvoke(_)
            | Event::ActionResult(_)
            | Event::ActionError(_)
            | Event::ShellCommandProgress(_)
            | Event::UiPromptSubmitted(_)
            | Event::AgentPromptQueued(_)
            | Event::AgentPromptRecalled(_)
            | Event::AgentPromptCreated(_)
            | Event::AgentPromptTerminated(_)
            | Event::AgentPromptPrewarmRequested(_)
            | Event::AgentState(_)
            | Event::AgentLoad(_)
            | Event::AgentLoading(_)
            | Event::AgentLoaded(_)
            | Event::AgentUnloaded(_)
            | Event::UiCompactRequest(_)
            | Event::UiCreateAgent(_)
            | Event::UiPromptDraft(_)
            | Event::UiFocusChanged(_)
            | Event::UiSetAgentDisplayName(_)
    )
}
