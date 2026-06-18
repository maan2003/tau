use tau_proto::{AgentStarted, Event, PromptOriginator, ToolError, ToolName, ToolType};

use super::semantic_event_router::should_persist;
use crate::parse_agent_id;

#[test]
fn event_classification_controls_persistence() {
    let event = Event::AgentStarted(AgentStarted {
        parent_agent: None,
        agent_id: parse_agent_id("agent-1"),
        role: "default".into(),
        display_name: None,
        metadata: Vec::new(),
    });

    assert!(should_persist(&event));
}

#[test]
fn terminal_tool_event_is_persisted() {
    let event = Event::ToolError(ToolError {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    });

    assert!(should_persist(&event));
}
