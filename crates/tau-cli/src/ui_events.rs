//! Shared constructors for UI protocol events.

use std::time::{SystemTime, UNIX_EPOCH};

use tau_proto::{Event, UiRoleUpdateAction};

pub(crate) fn cancel_prompt(target_agent_id: Option<tau_proto::AgentId>) -> Event {
    // Broadcast cancel within the selected agent conversation — abort whatever
    // is in flight there, regardless of spid. The targeted variant is used by
    // the harness for surgical preempts.
    Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
        target_agent_id,
        agent_prompt_id: None,
    })
}

pub(crate) fn tree_request(target_agent_id: Option<tau_proto::AgentId>) -> Event {
    Event::UiTreeRequest(tau_proto::UiTreeRequest { target_agent_id })
}

pub(crate) fn navigate_tree(target_agent_id: Option<tau_proto::AgentId>, node_id: u64) -> Event {
    Event::UiNavigateTree(tau_proto::UiNavigateTree {
        target_agent_id,
        node_id,
    })
}

pub(crate) fn compact_request(target_agent_id: Option<tau_proto::AgentId>) -> Event {
    Event::UiCompactRequest(tau_proto::UiCompactRequest { target_agent_id })
}

pub(crate) fn role_select(role: impl Into<String>) -> Event {
    Event::UiRoleSelect(tau_proto::UiRoleSelect { role: role.into() })
}

pub(crate) fn agent_model_select(
    target_agent_id: Option<tau_proto::AgentId>,
    model: tau_proto::ModelId,
) -> Event {
    Event::UiAgentModelSelect(tau_proto::UiAgentModelSelect {
        target_agent_id,
        model,
    })
}

pub(crate) fn role_update(role: impl Into<String>, action: UiRoleUpdateAction) -> Event {
    Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
        role: role.into(),
        action,
    })
}

pub(crate) fn shell_command(
    command: &str,
    include_in_context: bool,
    target_agent_id: Option<tau_proto::AgentId>,
) -> Event {
    let command_id = format!(
        "ui-sh-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    Event::UiShellCommand(tau_proto::UiShellCommand {
        command_id: command_id.into(),
        command: command.to_owned(),
        include_in_context,
        target_agent_id,
    })
}
