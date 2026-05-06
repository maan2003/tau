//! Slash-command and argument completion content + menu rendering.
//!
//! State and lifecycle live in [`tau_cli_term_raw`]; this module
//! supplies the *content* (which candidates exist for a given buffer)
//! and the *presentation* (how the menu block is laid out and styled).
//!
//! Public types:
//! - [`SlashCommand`] — static command registration
//! - [`CompletionItem`] / [`CompletionData`] — dynamic argument completions
//! - [`build_candidates`] — turns the current buffer into a `Vec<Candidate>`
//! - [`render_menu_block`] — turns a [`CompletionView`] into a [`StyledBlock`]

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use tau_cli_term_raw::{Candidate, CompletionView, Span, StyledBlock, StyledText};
use tau_themes::Theme;

use crate::resolve;

/// A slash-command name, always prefixed with `/` (e.g. `"/model"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommandName(String);

impl CommandName {
    pub fn new(name: impl Into<String>) -> Self {
        let s = name.into();
        debug_assert!(s.starts_with('/'), "CommandName must start with '/'");
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A slash command with its name and description.
#[derive(Clone, Debug)]
pub struct SlashCommand {
    pub name: CommandName,
    pub description: String,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: CommandName::new(name),
            description: description.into(),
        }
    }
}

/// A single argument completion candidate.
#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub value: String,
    pub description: String,
}

impl CompletionItem {
    pub fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }

    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: String::new(),
        }
    }
}

/// Thread-safe storage for dynamic argument completions.
///
/// Clone this handle and pass it to background threads that need to
/// update available completions (e.g. when the harness sends a model
/// list).
#[derive(Clone, Default)]
pub struct CompletionData {
    inner: Arc<Mutex<HashMap<CommandName, Vec<CompletionItem>>>>,
}

impl CompletionData {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the argument completions for a slash command.
    pub fn set_arg_completions(&self, command: CommandName, items: Vec<CompletionItem>) {
        self.inner
            .lock()
            .expect("completion data lock")
            .insert(command, items);
    }

    fn get_arg_completions(&self, command: &CommandName) -> Option<Vec<CompletionItem>> {
        self.inner
            .lock()
            .expect("completion data lock")
            .get(command)
            .cloned()
    }
}

/// Builds the candidate list for the given buffer/cursor.
///
/// - Buffer not starting with `/` → no candidates.
/// - Buffer with no space → match against the static slash-command registry by
///   prefix.
/// - Buffer with `<cmd> <arg-prefix>` → look up the dynamic `CompletionData`
///   for `<cmd>` and rank prefix matches before substring matches
///   (case-insensitive).
pub fn build_candidates(
    commands: &[SlashCommand],
    data: &CompletionData,
    buffer: &str,
    _cursor: usize,
) -> Vec<Candidate> {
    if !buffer.starts_with('/') {
        return Vec::new();
    }

    if let Some(space_pos) = buffer.find(' ') {
        let cmd = &buffer[..space_pos];
        let arg_prefix = &buffer[space_pos + 1..];
        build_arg_candidates(data, cmd, arg_prefix)
    } else {
        build_cmd_candidates(commands, buffer)
    }
}

fn build_cmd_candidates(commands: &[SlashCommand], prefix: &str) -> Vec<Candidate> {
    commands
        .iter()
        .filter(|cmd| cmd.name.as_str().starts_with(prefix))
        .map(|cmd| Candidate {
            label: cmd.name.to_string(),
            description: cmd.description.clone(),
            replacement: cmd.name.to_string(),
        })
        .collect()
}

fn build_arg_candidates(data: &CompletionData, cmd: &str, arg_prefix: &str) -> Vec<Candidate> {
    let cmd_name = CommandName::new(cmd);
    let Some(items) = data.get_arg_completions(&cmd_name) else {
        return Vec::new();
    };

    let needle = arg_prefix.to_lowercase();
    let mut prefix_matches = Vec::new();
    let mut substr_matches = Vec::new();

    for item in &items {
        let hay = item.value.to_lowercase();
        if needle.is_empty() || hay.starts_with(&needle) {
            prefix_matches.push(Candidate {
                label: item.value.clone(),
                description: item.description.clone(),
                replacement: format!("{cmd} {}", item.value),
            });
        } else if hay.contains(&needle) {
            substr_matches.push(Candidate {
                label: item.value.clone(),
                description: item.description.clone(),
                replacement: format!("{cmd} {}", item.value),
            });
        }
    }

    prefix_matches.extend(substr_matches);
    prefix_matches
}

/// Renders the completion menu as a [`StyledBlock`]: each candidate
/// on its own line, with the selected entry highlighted.
pub fn render_menu_block(view: &CompletionView, theme: &Theme) -> StyledBlock {
    let selected_style = resolve::resolve(theme, tau_themes::names::COMPLETION_SELECTED);
    let label_style = resolve::resolve(theme, tau_themes::names::COMPLETION_LABEL);
    let desc_style = resolve::resolve(theme, tau_themes::names::COMPLETION_DESC);

    let max_label_len = view
        .candidates
        .iter()
        .map(|c| c.label.len())
        .max()
        .unwrap_or(0);

    let mut spans: Vec<Span> = Vec::new();
    for (i, candidate) in view.candidates.iter().enumerate() {
        if i > 0 {
            spans.push(Span::plain("\n"));
        }

        let is_selected = view.selected == Some(i);
        let padding = max_label_len - candidate.label.len() + 2;

        let line_text = if candidate.description.is_empty() {
            format!("  {}  ", candidate.label)
        } else {
            format!(
                "  {}{:padding$}{}  ",
                candidate.label,
                "",
                candidate.description,
                padding = padding,
            )
        };

        if is_selected {
            spans.push(Span::new(line_text, selected_style));
        } else {
            spans.push(Span::plain("  "));
            spans.push(Span::new(&candidate.label, label_style));
            if !candidate.description.is_empty() {
                spans.push(Span::plain(format!("{:padding$}", "", padding = padding)));
                spans.push(Span::new(&candidate.description, desc_style));
            }
            spans.push(Span::plain("  "));
        }
    }

    StyledBlock::new(StyledText::from(spans))
}
