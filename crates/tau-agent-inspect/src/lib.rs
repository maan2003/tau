//! Read-only agent/policy inspection for CLI sub-commands and scripts.
//!
//! Operates entirely on `tau-core` types and the on-disk agent/policy
//! format. Intentionally has no dependency on the harness daemon, so
//! `tau policy list` / similar commands don't drag
//! in the agent, extension supervisor, or event-loop graph just to
//! render an events.jsonl.

use std::path::{Path, PathBuf};
use std::{fmt, io};

use tau_core::{AgentEntry, PolicyStore, PolicyStoreError};
use tau_proto::{
    CborValue, ContentPart, ContextItem, EventSelector, ToolCallItem, ToolResultStatus,
};

/// Errors from the read-only inspection paths.
#[derive(Debug)]
pub enum InspectError {
    Io(io::Error),
    PolicyStore(PolicyStoreError),
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::PolicyStore(source) => write!(f, "policy store error: {source}"),
        }
    }
}

impl std::error::Error for InspectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::PolicyStore(source) => Some(source),
        }
    }
}

impl From<io::Error> for InspectError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<PolicyStoreError> for InspectError {
    fn from(source: PolicyStoreError) -> Self {
        Self::PolicyStore(source)
    }
}

/// Returns the default per-state directory: `$XDG_STATE_HOME/tau` (typically
/// `~/.local/state/tau` on Linux), or `.tau/state` if no state dir is
/// available.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    tau_config::settings::state_dir().unwrap_or_else(|| PathBuf::from(".tau").join("state"))
}

pub fn open_policy_store(path: impl AsRef<Path>) -> Result<PolicyStore, InspectError> {
    PolicyStore::open(path.as_ref()).map_err(InspectError::from)
}

pub fn policy_lines(path: impl AsRef<Path>) -> Result<Vec<String>, InspectError> {
    let store = open_policy_store(path)?;
    let mut approvals = store.approvals().to_vec();
    approvals.sort_by(|a, b| a.connection_name.cmp(&b.connection_name));
    if approvals.is_empty() {
        return Ok(vec!["no policy approvals".to_owned()]);
    }
    Ok(approvals
        .into_iter()
        .map(|a| {
            let sels = a
                .selectors
                .iter()
                .map(|s| match s {
                    EventSelector::Exact(n) => n.to_string(),
                    EventSelector::Prefix(p) => format!("{p}*"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} [{:?}] -> {sels}",
                a.connection_name, a.connection_origin
            )
        })
        .collect())
}

/// Pretty-print one agent entry for line-oriented inspection output (`/tree`,
/// debug log).
#[must_use]
pub fn format_agent_entry(entry: &AgentEntry) -> String {
    match entry {
        AgentEntry::UserInput { items } => {
            format!("user: {}", first_message_text(items).unwrap_or_default())
        }
        AgentEntry::AssistantResponse { output_items, .. } => {
            let body =
                assistant_output_preview(output_items).unwrap_or_else(|| "(no text)".to_owned());
            format!("agent: {body}")
        }
        AgentEntry::ToolResults { items } => {
            if items.is_empty() {
                return "tool.result (empty)".to_owned();
            };
            items
                .iter()
                .map(format_tool_result_item)
                .collect::<Vec<_>>()
                .join("; ")
        }
        AgentEntry::AgentMessage {
            direction, message, ..
        } => {
            let event_name = match direction {
                tau_core::AgentMessageDirection::Outbound => "agent.message_sent",
                tau_core::AgentMessageDirection::Inbound => "agent.message_received",
            };
            format!("{event_name}: {message}")
        }
    }
}

fn format_tool_result_item(item: &tau_proto::ToolResultItem) -> String {
    match &item.status {
        ToolResultStatus::Success => {
            let preview = truncate_chars(&item.output.render(), 80);
            format!("tool.result {} -> {preview}", item.call_id)
        }
        ToolResultStatus::Error { message } => {
            format!("tool.error {} -> {message}", item.call_id)
        }
        ToolResultStatus::Cancelled { reason } => {
            format!("tool.cancelled {} -> {reason}", item.call_id)
        }
    }
}

#[must_use]
fn assistant_output_preview(items: &[ContextItem]) -> Option<String> {
    let parts = items
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(_) => first_message_text(std::slice::from_ref(item)),
            ContextItem::ToolCall(call) => Some(tool_call_preview(call)),
            ContextItem::CompactionTrigger => Some("manual compaction requested".to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then_some(parts.join(" "))
}

fn tool_call_preview(call: &ToolCallItem) -> String {
    let args = match call.arguments {
        CborValue::Map(ref entries) => entries.iter().find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Text(value))
                if matches!(key.as_str(), "path" | "pattern" | "command" | "task_name") =>
            {
                Some(value.clone())
            }
            _ => None,
        }),
        _ => None,
    };
    match args {
        Some(args) if !args.is_empty() => format!("tool.call {} {args}", call.name),
        _ => format!("tool.call {}", call.name),
    }
}

fn first_message_text(items: &[ContextItem]) -> Option<String> {
    items.iter().find_map(|item| match item {
        ContextItem::Message(message) => {
            let mut text = String::new();
            for part in &message.content {
                let ContentPart::Text { text: part } = part;
                text.push_str(part);
            }
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests;
