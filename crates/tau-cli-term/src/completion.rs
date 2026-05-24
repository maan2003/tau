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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tau_cli_term_raw::{Candidate, CompletionView, Span, StyledBlock, StyledText};
use tau_themes::Theme;
use unicode_width::UnicodeWidthStr;

use crate::resolve;

/// A slash-command name, always prefixed with `/` (e.g. `"/model"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommandName(String);

impl CommandName {
    pub fn new(name: impl Into<String>) -> Self {
        let s = name.into();
        assert!(s.starts_with('/'), "CommandName must start with '/'");
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

/// Closure that produces argument completions for a slash command,
/// given the already-typed args (the last element is the partial arg
/// being completed; may be empty for "just typed a space").
///
/// The closure is responsible for filtering and ranking — callers do
/// no further processing. For the common flat-list case use
/// [`CompletionData::set_arg_completions`], which builds an appropriate
/// closure internally.
pub type ArgCompleter = Arc<dyn Fn(&[&str]) -> Vec<CompletionItem> + Send + Sync>;

/// Mutable completion state shared with background renderer updates.
#[derive(Default)]
struct CompletionInner {
    arg_completers: HashMap<CommandName, ArgCompleter>,
    dynamic_commands: Vec<SlashCommand>,
}

/// Thread-safe storage for dynamic slash-command and argument completions.
///
/// Clone this handle and pass it to background threads that need to
/// update available completions (e.g. when the harness sends a model
/// list or an extension publishes an action schema).
#[derive(Clone, Default)]
pub struct CompletionData {
    inner: Arc<Mutex<CompletionInner>>,
}

impl CompletionData {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces extension-provided root slash commands shown alongside the
    /// static command registry.
    pub fn set_dynamic_commands(&self, commands: Vec<SlashCommand>) {
        self.inner
            .lock()
            .expect("completion data lock")
            .dynamic_commands = commands;
    }

    /// Sets a flat, single-arg completion list for a slash command.
    /// Items are ranked prefix-match-first, substring-match-second
    /// (case-insensitive). For commands that take more than one arg
    /// or need to react to prior args, use
    /// [`CompletionData::set_arg_completer`].
    pub fn set_arg_completions(&self, command: CommandName, items: Vec<CompletionItem>) {
        // Precompute lowercased haystacks once at insertion time so
        // the per-keystroke match loop doesn't reallocate.
        let indexed: Arc<Vec<(CompletionItem, String)>> = Arc::new(
            items
                .into_iter()
                .map(|item| {
                    let lower = item.value.to_lowercase();
                    (item, lower)
                })
                .collect(),
        );
        let completer: ArgCompleter = Arc::new(move |args: &[&str]| {
            // Single-arg completion only — multi-arg buffers fall
            // through to no candidates.
            if args.len() != 1 {
                return Vec::new();
            }
            let needle = args[0].to_lowercase();
            let mut prefix_matches = Vec::new();
            let mut substr_matches = Vec::new();
            for (item, value_lower) in indexed.iter() {
                if needle.is_empty() || value_lower.starts_with(&needle) {
                    prefix_matches.push(item.clone());
                } else if value_lower.contains(&needle) {
                    substr_matches.push(item.clone());
                }
            }
            prefix_matches.extend(substr_matches);
            prefix_matches
        });
        self.inner
            .lock()
            .expect("completion data lock")
            .arg_completers
            .insert(command, completer);
    }

    /// Registers a custom argument completer for a slash command.
    /// The closure receives the args typed so far (with the partial
    /// last element being completed) and returns ranked candidates.
    pub fn set_arg_completer(&self, command: CommandName, completer: ArgCompleter) {
        self.inner
            .lock()
            .expect("completion data lock")
            .arg_completers
            .insert(command, completer);
    }

    fn get_arg_completer(&self, command: &CommandName) -> Option<ArgCompleter> {
        self.inner
            .lock()
            .expect("completion data lock")
            .arg_completers
            .get(command)
            .cloned()
    }

    fn dynamic_commands(&self) -> Vec<SlashCommand> {
        self.inner
            .lock()
            .expect("completion data lock")
            .dynamic_commands
            .clone()
    }
}

/// Builds the candidate list for the given buffer/cursor.
///
/// - Buffer starting with `./`, `../`, `~`, or `~/` → filesystem path
///   candidates.
/// - Buffer not starting with `/` → no slash-command candidates.
/// - Buffer with no space → match against the static slash-command registry by
///   prefix.
/// - Buffer with `<cmd> <arg-prefix>` → look up the dynamic `CompletionData`
///   for `<cmd>` and rank prefix matches before substring matches
///   (case-insensitive).
pub fn build_candidates(
    commands: &[SlashCommand],
    data: &CompletionData,
    buffer: &str,
    cursor: usize,
) -> Vec<Candidate> {
    build_candidates_with_home(commands, data, buffer, cursor, home_dir().as_deref())
}

pub(crate) fn build_candidates_with_home(
    commands: &[SlashCommand],
    data: &CompletionData,
    buffer: &str,
    cursor: usize,
    home_dir: Option<&Path>,
) -> Vec<Candidate> {
    if let Some(path_token) = filesystem_path_token(buffer, cursor) {
        return build_filesystem_candidates_with_home(&path_token, home_dir);
    }

    if !buffer.starts_with('/') {
        return Vec::new();
    }

    if let Some(space_pos) = buffer.find(' ') {
        let cmd = &buffer[..space_pos];
        let rest = &buffer[space_pos + 1..];
        build_arg_candidates(data, cmd, rest)
    } else {
        build_cmd_candidates(commands, &data.dynamic_commands(), buffer)
    }
}

fn build_cmd_candidates(
    static_commands: &[SlashCommand],
    dynamic_commands: &[SlashCommand],
    prefix: &str,
) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    static_commands
        .iter()
        .chain(dynamic_commands)
        .filter(|cmd| seen.insert(cmd.name.to_string()))
        .filter(|cmd| cmd.name.as_str().starts_with(prefix))
        .map(|cmd| Candidate {
            label: cmd.name.to_string(),
            description: cmd.description.clone(),
            replacement: cmd.name.to_string(),
        })
        .collect()
}

struct PathToken<'a> {
    prefix: &'a str,
    before: &'a str,
    after: &'a str,
}

fn filesystem_path_token(buffer: &str, cursor: usize) -> Option<PathToken<'_>> {
    let before_cursor = buffer.get(..cursor)?;
    let after_cursor = buffer.get(cursor..)?;
    let token_start = before_cursor
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(idx + ch.len_utf8()))
        .unwrap_or(0);
    let token_end = after_cursor
        .char_indices()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(cursor + idx))
        .unwrap_or(buffer.len());
    let prefix = &buffer[token_start..cursor];
    if is_filesystem_prefix(prefix) {
        Some(PathToken {
            prefix,
            before: &buffer[..token_start],
            after: &buffer[token_end..],
        })
    } else {
        None
    }
}

fn is_filesystem_prefix(buffer: &str) -> bool {
    buffer.starts_with("./")
        || buffer.starts_with("../")
        || buffer == "~"
        || buffer.starts_with("~/")
}

fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.as_os_str().is_empty() {
        None
    } else {
        Some(PathBuf::from(home))
    }
}

fn home_expanded_path(prefix: &str, home_dir: Option<&Path>) -> Option<PathBuf> {
    if prefix == "~" {
        Some(home_dir?.to_path_buf())
    } else if let Some(rest) = prefix.strip_prefix("~/") {
        Some(home_dir?.join(rest))
    } else {
        Some(PathBuf::from(prefix))
    }
}

fn build_filesystem_candidates_with_home(
    path_token: &PathToken<'_>,
    home_dir: Option<&Path>,
) -> Vec<Candidate> {
    let prefix = path_token.prefix;
    let Some(lookup_path) = home_expanded_path(prefix, home_dir) else {
        return Vec::new();
    };
    let display_path = Path::new(prefix);
    let (lookup_dir, display_dir, partial) = if prefix == "~" {
        (lookup_path, PathBuf::from("~"), "")
    } else if prefix.ends_with('/') {
        (lookup_path, display_path.to_path_buf(), "")
    } else {
        let Some(lookup_parent) = lookup_path.parent() else {
            return Vec::new();
        };
        let Some(display_parent) = display_path.parent() else {
            return Vec::new();
        };
        let partial = display_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let lookup_dir = if lookup_parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            lookup_parent.to_path_buf()
        };
        let display_dir = if display_parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            display_parent.to_path_buf()
        };
        (lookup_dir, display_dir, partial)
    };

    let Ok(entries) = std::fs::read_dir(lookup_dir) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(partial) {
            continue;
        }
        if !partial.starts_with('.') && name.starts_with('.') {
            continue;
        }

        let is_dir = entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
        let replacement = display_dir.join(name).to_string_lossy().into_owned();
        candidates.push(Candidate {
            label: replacement.clone(),
            description: if is_dir { "directory" } else { "file" }.to_owned(),
            replacement: format!("{}{}{}", path_token.before, replacement, path_token.after),
        });
    }

    candidates.sort_by(|a, b| a.label.cmp(&b.label));
    candidates
}

fn build_arg_candidates(data: &CompletionData, cmd: &str, rest: &str) -> Vec<Candidate> {
    let cmd_name = CommandName::new(cmd);
    let Some(completer) = data.get_arg_completer(&cmd_name) else {
        return Vec::new();
    };

    // Split args on whitespace, but preserve a trailing empty arg
    // when the buffer ends in a space — that's the position the user
    // is currently completing (e.g. "/set show-diff " → args
    // ["show-diff", ""]).
    let args: Vec<&str> = if rest.is_empty() {
        vec![""]
    } else if rest.ends_with(' ') {
        let mut v: Vec<&str> = rest.split_whitespace().collect();
        v.push("");
        v
    } else {
        rest.split_whitespace().collect()
    };

    // Everything up to and including the last *completed* token is
    // preserved as the replacement prefix — completion replaces the
    // final, partial token.
    let prefix = if args.len() <= 1 {
        cmd.to_owned()
    } else {
        format!("{cmd} {}", args[..args.len() - 1].join(" "))
    };

    completer(&args)
        .into_iter()
        .map(|item| Candidate {
            label: item.value.clone(),
            description: item.description.clone(),
            replacement: format!("{prefix} {}", item.value),
        })
        .collect()
}

const COMPLETION_MENU_MAX_HEIGHT_PERCENT: usize = 30;

/// Renders the completion menu as a [`StyledBlock`]: each candidate
/// on its own line, with the selected entry highlighted.
pub fn render_menu_block(
    view: &CompletionView,
    theme: &Theme,
    terminal_width: usize,
    terminal_height: usize,
) -> StyledBlock {
    render_menu_block_with_max_rows(
        view,
        theme,
        terminal_width,
        completion_menu_max_rows(terminal_height),
    )
}

fn completion_menu_max_rows(terminal_height: usize) -> usize {
    (terminal_height * COMPLETION_MENU_MAX_HEIGHT_PERCENT / 100).max(1)
}

fn visible_candidate_range(view: &CompletionView, max_rows: usize) -> std::ops::Range<usize> {
    let total = view.candidates.len();
    let max_rows = max_rows.max(1).min(total.max(1));
    if total <= max_rows {
        return 0..total;
    }

    let selected = view.selected.unwrap_or(0).min(total - 1);
    let half = max_rows / 2;
    let start = selected.saturating_sub(half).min(total - max_rows);
    start..start + max_rows
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 1 {
        return "…".to_owned();
    }

    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if max_width - 1 < width + ch_width {
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out.push('…');
    out
}

struct MenuLineParts {
    label: String,
    padding: usize,
    description: String,
}

fn menu_line_parts(
    candidate: &Candidate,
    max_label_width: usize,
    terminal_width: usize,
) -> MenuLineParts {
    let inner_width = terminal_width.max(1).saturating_sub(4);
    let label_budget = max_label_width.min(inner_width);
    let label = truncate_to_width(&candidate.label, label_budget);
    let label_width = UnicodeWidthStr::width(label.as_str());
    let remaining = inner_width.saturating_sub(label_width);

    let mut padding = 0;
    let mut description = String::new();
    if !candidate.description.is_empty() && 0 < remaining {
        padding = (max_label_width.saturating_sub(label_width) + 2).min(remaining);
        let desc_budget = remaining.saturating_sub(padding);
        if 0 < desc_budget {
            description = truncate_to_width(&candidate.description, desc_budget);
        }
    }

    MenuLineParts {
        label,
        padding,
        description,
    }
}

fn render_menu_block_with_max_rows(
    view: &CompletionView,
    theme: &Theme,
    terminal_width: usize,
    max_rows: usize,
) -> StyledBlock {
    let selected_style = resolve::resolve(theme, tau_themes::names::COMPLETION_SELECTED);
    let label_style = resolve::resolve(theme, tau_themes::names::COMPLETION_LABEL);
    let desc_style = resolve::resolve(theme, tau_themes::names::COMPLETION_DESC);

    let visible = visible_candidate_range(view, max_rows);
    let max_label_width = view.candidates[visible.clone()]
        .iter()
        .map(|c| UnicodeWidthStr::width(c.label.as_str()))
        .max()
        .unwrap_or(0);

    let mut spans: Vec<Span> = Vec::new();
    for (row, i) in visible.enumerate() {
        let candidate = &view.candidates[i];
        if row > 0 {
            spans.push(Span::plain("\n"));
        }

        let is_selected = view.selected == Some(i);
        let parts = menu_line_parts(candidate, max_label_width, terminal_width);

        let line_text = if parts.description.is_empty() {
            format!("  {}  ", parts.label)
        } else {
            format!(
                "  {}{:padding$}{}  ",
                parts.label,
                "",
                parts.description,
                padding = parts.padding,
            )
        };

        if is_selected {
            spans.push(Span::new(line_text, selected_style));
        } else {
            spans.push(Span::plain("  "));
            spans.push(Span::new(parts.label, label_style));
            if !parts.description.is_empty() {
                spans.push(Span::plain(format!(
                    "{:padding$}",
                    "",
                    padding = parts.padding
                )));
                spans.push(Span::new(parts.description, desc_style));
            }
            spans.push(Span::plain("  "));
        }
    }

    StyledBlock::new(StyledText::from(spans))
}

#[cfg(test)]
mod render_tests;
