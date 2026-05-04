//! Pretty-printing helpers for events and session entries — used by the
//! CLI inspection commands and the daemon's lifecycle-message stream.

use tau_core::{SessionEntry, SessionTree, ToolActivityOutcome};
use tau_proto::{Event, ProgressUpdate, ToolProgress};

use crate::prompt::{cbor_map_text, cbor_to_text};

/// Formats a tool progress event for display.
#[must_use]
pub fn format_tool_progress(progress: &ToolProgress) -> String {
    let mut text = progress.tool_name.to_string();
    if let Some(message) = &progress.message {
        text.push_str(": ");
        text.push_str(message);
    }
    if let Some(ProgressUpdate {
        current: Some(current),
        total: Some(total),
    }) = &progress.progress
    {
        text.push_str(&format!(" ({current}/{total})"));
    }
    text
}

/// Formats an extension lifecycle event for display.
#[must_use]
pub fn format_extension_event(event: &Event) -> String {
    match event {
        Event::ExtensionStarting(s) => format!("extension {} starting", s.extension_name),
        Event::ExtensionReady(r) => format!("extension {} ready", r.extension_name),
        Event::ExtensionExited(e) => format!("extension {} exited", e.extension_name),
        Event::ExtensionRestarting(r) => format!("extension {} restarting", r.extension_name),
        _ => event.name().to_string(),
    }
}

pub(crate) fn format_session_entry(entry: &SessionEntry) -> String {
    match entry {
        SessionEntry::UserMessage { text } => format!("user: {text}"),
        SessionEntry::AgentMessage { text, .. } => format!("agent: {text}"),
        SessionEntry::ToolActivity(a) => match &a.outcome {
            ToolActivityOutcome::Requested { arguments } => {
                if a.tool_name.as_str() == "skill" {
                    let name = cbor_map_text(arguments, "name").unwrap_or_default();
                    if name.is_empty() {
                        "tool.request skill".to_owned()
                    } else {
                        format!("tool.request skill {name}")
                    }
                } else {
                    format!("tool.request {}", a.tool_name)
                }
            }
            ToolActivityOutcome::Result { result } => {
                let text = cbor_to_text(result);
                let preview = if text.len() > 80 {
                    format!("{}...", &text[..80])
                } else {
                    text
                };
                format!("tool.result {} -> {preview}", a.tool_name)
            }
            ToolActivityOutcome::Error { message, .. } => {
                format!("tool.error {} -> {message}", a.tool_name)
            }
        },
    }
}

/// One-line preview of a session entry for `/tree` output.
pub(crate) fn render_entry_preview(entry: &SessionEntry) -> String {
    let raw = format_session_entry(entry);
    let single_line: String = raw
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    if single_line.chars().count() > 60 {
        let truncated: String = single_line.chars().take(60).collect();
        format!("{truncated}…")
    } else {
        single_line
    }
}

pub(crate) fn latest_agent_preview(session: &SessionTree) -> Option<String> {
    session
        .current_branch()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            SessionEntry::AgentMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
}
