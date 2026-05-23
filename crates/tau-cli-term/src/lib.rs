//! Higher-level terminal prompt with slash-command completion.
//!
//! This crate is now a thin shell around [`tau_cli_term_raw`]: the
//! raw layer owns the input state machine (history navigation,
//! completion menu lifecycle, key dispatch). This crate plugs in the
//! *content* (which candidates exist for a given buffer) and the
//! *presentation* (how the menu is rendered as a styled block under
//! the prompt). It also handles `$EDITOR` integration, which doesn't
//! belong in the raw layer.

pub mod completion;
pub mod resolve;
#[cfg(test)]
mod tests;

use std::io::{self, Write as _};
use std::sync::{Arc, Mutex};

pub use completion::{ArgCompleter, CommandName, CompletionData, CompletionItem, SlashCommand};
#[cfg(test)]
pub(crate) use tau_cli_term_raw::RawEvent as TestRawEvent;
pub use tau_cli_term_raw::{
    Align, BlockId, Cell, Color, CursorShape, Span, Style, StyledBlock, StyledText, TermHandle,
};
use tau_cli_term_raw::{Candidate, Event as RawEvent};
use tau_themes::Theme;

const PROMPT_TRAILER_MARKER: &str =
    "<!-- TAU trailer: everything after this line will be ignored -->";

/// High-level events surfaced to the caller.
pub enum Event {
    /// The user submitted a line (pressed Ctrl-Enter or ran `submit-prompt`
    /// with no completion preview).
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
    /// The user requested prompt cancellation with a second consecutive Ctrl-C.
    CancelPrompt,
    /// The terminal was resized.
    Resize { width: u16, height: u16 },
    /// The input buffer changed (or the completion menu cycled,
    /// opened, or closed). Caller should redraw any prompt-derived
    /// UI.
    BufferChanged,
    /// Shift+Tab pressed outside an open completion menu.
    BackTab,
    /// Escape pressed outside an open completion menu.
    Escape,
    /// A binding requested Fast mode toggle without touching the prompt draft.
    FastToggle,
    /// A binding requested cycling within the current agent role group.
    CycleRole,
    /// A binding requested cycling to the next agent role group.
    CycleRoleGroup,
}

/// Higher-level terminal prompt with completion support.
pub struct HighTerm {
    term: tau_cli_term_raw::Term,
    handle: TermHandle,
    theme: Theme,
    editor_context: Arc<Mutex<EditorContext>>,
    /// Editor command resolved once at startup: `$EDITOR`, else
    /// `$VISUAL`, else the first of `vim`/`vi`/`nano` found on
    /// `$PATH`. Passed to shell actions as `$TAU_EDITOR`.
    external_editor: Option<String>,
    /// Block id for the completion menu, allocated lazily on first
    /// open. Reused across opens; content swapped to empty when the
    /// menu is hidden.
    menu_block_id: Option<BlockId>,
    /// Submitted prompt history used by prompt-history search. Seeded
    /// from persistent history at startup and extended with submitted
    /// prompts from this process.
    prompt_history: Vec<String>,
}

impl HighTerm {
    /// Creates a new terminal with the given prompt and slash commands.
    ///
    /// Returns the terminal, a thread-safe handle for rendering, and a
    /// [`CompletionData`] handle for pushing dynamic argument completions
    /// from background threads.
    pub fn new(
        left_prompt: impl Into<StyledText>,
        commands: Vec<SlashCommand>,
        theme: Theme,
        cursor_shape: CursorShape,
        bindings: impl IntoIterator<Item = (String, String)>,
    ) -> io::Result<(Self, TermHandle, CompletionData)> {
        Self::new_with_input_history(
            left_prompt,
            commands,
            theme,
            cursor_shape,
            bindings,
            std::iter::empty(),
        )
    }

    /// Creates a new terminal and seeds prompt input history.
    pub fn new_with_input_history(
        left_prompt: impl Into<StyledText>,
        commands: Vec<SlashCommand>,
        theme: Theme,
        cursor_shape: CursorShape,
        bindings: impl IntoIterator<Item = (String, String)>,
        input_history: impl IntoIterator<Item = String>,
    ) -> io::Result<(Self, TermHandle, CompletionData)> {
        let input_history: Vec<String> = input_history.into_iter().collect();
        let (mut term, handle) = tau_cli_term_raw::Term::new(left_prompt, cursor_shape)?;
        term.seed_input_history(input_history.clone());
        term.set_bindings(bindings);
        let handle_clone = handle.clone();
        let data = CompletionData::new();
        let data_clone = data.clone();
        term.set_completion_source(Some(make_completion_source(commands, data)));
        let external_editor = resolve_external_editor();
        Ok((
            Self {
                term,
                handle,
                theme,
                editor_context: Arc::new(Mutex::new(EditorContext::default())),
                external_editor,
                menu_block_id: None,
                prompt_history: input_history
                    .into_iter()
                    .filter(|entry| !entry.is_empty())
                    .collect(),
            },
            handle_clone,
            data_clone,
        ))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        mut term: tau_cli_term_raw::Term,
        handle: TermHandle,
        commands: Vec<SlashCommand>,
        theme: Theme,
        bindings: impl IntoIterator<Item = (String, String)>,
    ) -> (Self, CompletionData) {
        let data = CompletionData::new();
        let data_clone = data.clone();
        term.set_completion_source(Some(make_completion_source(commands, data)));
        term.set_bindings(bindings);
        (
            Self {
                term,
                handle,
                theme,
                editor_context: Arc::new(Mutex::new(EditorContext::default())),
                external_editor: None,
                menu_block_id: None,
                prompt_history: Vec::new(),
            },
            data_clone,
        )
    }

    /// Returns a reference to the [`TermHandle`].
    pub fn handle(&self) -> &TermHandle {
        &self.handle
    }

    /// Replaces the editor-context storage with a shared handle.
    ///
    /// Use this when another component (e.g. the event renderer) owns
    /// the authoritative context and needs the prompt's external-editor
    /// integration to read from the same `Arc`. The previously-owned
    /// `EditorContext` is dropped.
    pub fn set_editor_context_handle(&mut self, editor_context: Arc<Mutex<EditorContext>>) {
        self.editor_context = editor_context;
    }

    /// Triggers a redraw.
    pub fn redraw(&self) {
        self.handle.redraw();
    }

    /// Appends persistent output to history.
    pub fn print_output(
        &self,
        debug_id: impl Into<String>,
        block: impl Into<StyledBlock>,
    ) -> BlockId {
        self.handle.print_output(debug_id, block)
    }

    /// Blocks until the next high-level event, syncing the
    /// completion menu block to the raw term's current state.
    pub fn get_next_event(&mut self) -> io::Result<Event> {
        loop {
            let raw = self.term.get_next_event()?;

            match raw {
                RawEvent::BufferChanged => {
                    self.sync_menu_block();
                    self.handle.redraw();
                    return Ok(Event::BufferChanged);
                }

                RawEvent::CompletionAccept => {
                    // Accept-without-submit: the buffer already
                    // reflects the chosen candidate. Sync the menu
                    // (now closed) and loop so the user has to press
                    // Enter again to actually submit.
                    self.sync_menu_block();
                    self.handle.redraw();
                    continue;
                }

                RawEvent::BackTab => return Ok(Event::BackTab),

                RawEvent::Escape => return Ok(Event::Escape),

                RawEvent::Line(line) => {
                    if !line.is_empty() {
                        self.prompt_history.push(line.clone());
                    }
                    self.sync_menu_block();
                    self.handle.redraw();
                    return Ok(Event::Line(line));
                }

                RawEvent::Eof => {
                    self.sync_menu_block();
                    return Ok(Event::Eof);
                }

                RawEvent::CancelPrompt => {
                    self.sync_menu_block();
                    self.handle.redraw_sync();
                    return Ok(Event::CancelPrompt);
                }

                RawEvent::Resize { width, height } => {
                    self.sync_menu_block();
                    self.handle.redraw();
                    return Ok(Event::Resize { width, height });
                }

                RawEvent::Notice(message) => {
                    self.sync_menu_block();
                    self.print_local(&message);
                    self.handle.redraw_sync();
                    return Ok(Event::BufferChanged);
                }

                RawEvent::ExternalEditor => {
                    self.sync_menu_block();
                    let outcome =
                        self.run_prompt_action(PromptShellAction::Edit(PromptShellCommand {
                            command: "$TAU_EDITOR \"$TAU_PROMPT_PATH\"".to_owned(),
                            trim: false,
                        }));
                    self.handle.redraw_sync();
                    match outcome {
                        PromptActionOutcome::BufferChanged => return Ok(Event::BufferChanged),
                        PromptActionOutcome::Continue => continue,
                        PromptActionOutcome::Return(event) => return Ok(event),
                    }
                }

                RawEvent::Binding(action) => {
                    self.sync_menu_block();
                    let outcome = self.run_binding(&action);
                    self.handle.redraw_sync();
                    match outcome {
                        PromptActionOutcome::BufferChanged => return Ok(Event::BufferChanged),
                        PromptActionOutcome::Continue => continue,
                        PromptActionOutcome::Return(event) => return Ok(event),
                    }
                }
            }
        }
    }

    /// Updates the suggestion block to match the raw term's
    /// completion state: renders the menu when one is open, hides
    /// the block otherwise.
    fn sync_menu_block(&mut self) {
        match self.term.completion_state() {
            Some(view) => {
                let (width, height) = self.handle.size();
                let block = completion::render_menu_block(&view, &self.theme, width, height);
                let id = match self.menu_block_id {
                    Some(id) => id,
                    None => {
                        let id = self.handle.new_block("completion-menu", "");
                        self.handle.push_suggestions(id);
                        self.menu_block_id = Some(id);
                        id
                    }
                };
                self.handle.set_block(id, block);
            }
            None => {
                if let Some(id) = self.menu_block_id.take() {
                    self.handle.remove_suggestions(id);
                    self.handle.remove_block(id);
                }
            }
        }
    }

    fn run_binding(&mut self, action: &str) -> PromptActionOutcome {
        tracing::trace!(target: "tau_cli::input", action, "running prompt binding");
        let Some(action) = PromptShellAction::parse(action) else {
            self.print_local(&format!("binding: unknown action `{action}`"));
            return PromptActionOutcome::BufferChanged;
        };
        self.run_prompt_action(action)
    }

    /// Runs a [`PromptShellAction`] and applies its result to the
    /// input buffer. Errors (spawn failure, bad utf-8, no editor)
    /// surface as a themed info line above the prompt.
    fn run_prompt_action(&mut self, action: PromptShellAction) -> PromptActionOutcome {
        match run_prompt_shell_action(
            &self.term,
            &self.handle,
            self.editor_context.clone(),
            self.external_editor.as_deref(),
            &self.prompt_history,
            action,
        ) {
            Ok(Some(PromptShellResult::Replace(new_text))) => {
                let cursor = new_text.len();
                self.handle.set_buffer(new_text, cursor);
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::ReplacePreservingUndo(new_text))) => {
                let cursor = new_text.len();
                self.handle.set_buffer_preserving_undo(new_text, cursor);
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::Insert(text))) => {
                let mut buffer = self.handle.get_buffer();
                let cursor = self.handle.get_cursor();
                buffer.insert_str(cursor, &text);
                self.handle.set_buffer(buffer, cursor + text.len());
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::FastToggle)) => {
                return PromptActionOutcome::Return(Event::FastToggle);
            }
            Ok(Some(PromptShellResult::CycleRole)) => {
                return PromptActionOutcome::Return(Event::CycleRole);
            }
            Ok(Some(PromptShellResult::CycleRoleGroup)) => {
                return PromptActionOutcome::Return(Event::CycleRoleGroup);
            }
            Ok(Some(PromptShellResult::History(delta))) => {
                self.term.trigger_history_step(delta);
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::Undo)) => {
                self.term.trigger_undo();
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::Redo)) => {
                self.term.trigger_redo();
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::RawEvent(raw))) => {
                return self.apply_raw_prompt_event(raw);
            }
            Ok(None) => {} // shell exited non-zero or no output applies.
            Err(msg) => self.print_local(&format!("prompt action: {msg}")),
        }
        PromptActionOutcome::BufferChanged
    }

    fn apply_raw_prompt_event(&mut self, raw: RawEvent) -> PromptActionOutcome {
        match raw {
            RawEvent::BufferChanged => {
                self.sync_menu_block();
                PromptActionOutcome::BufferChanged
            }
            RawEvent::CompletionAccept => {
                self.sync_menu_block();
                PromptActionOutcome::Continue
            }
            RawEvent::Line(line) => {
                if !line.is_empty() {
                    self.prompt_history.push(line.clone());
                }
                self.sync_menu_block();
                PromptActionOutcome::Return(Event::Line(line))
            }
            RawEvent::Eof
            | RawEvent::CancelPrompt
            | RawEvent::Resize { .. }
            | RawEvent::BackTab
            | RawEvent::Escape
            | RawEvent::Binding(_)
            | RawEvent::Notice(_)
            | RawEvent::ExternalEditor => unreachable!("unsupported prompt action event"),
        }
    }

    fn print_local(&self, message: &str) {
        let block = resolve::themed_block(
            &self.theme,
            tau_themes::names::SYSTEM_INFO,
            message.to_owned(),
        );
        self.handle.print_output("prompt-action-error", block);
    }
}

fn make_completion_source(
    commands: Vec<SlashCommand>,
    data: CompletionData,
) -> Box<dyn tau_cli_term_raw::CompletionSource> {
    let commands = Arc::new(commands);
    Box::new(move |buffer: &str, cursor: usize| -> Vec<Candidate> {
        completion::build_candidates(&commands, &data, buffer, cursor)
    })
}

struct PromptShellCommand {
    command: String,
    trim: bool,
}

enum PromptShellAction {
    Insert(PromptShellCommand),
    Edit(PromptShellCommand),
    HistorySearch(PromptShellCommand),
    FastToggle,
    CycleRole,
    CycleRoleGroup,
    PromptNext,
    PromptPrevious,
    PromptUndo,
    PromptRedo,
    SubmitPrompt,
    InsertNewline,
}

#[derive(Clone, Default)]
pub struct EditorContext {
    pub active_prompt: Option<String>,
    pub last_agent_response: Option<String>,
    pub previous_prompt: Option<String>,
}

enum PromptShellResult {
    Insert(String),
    Replace(String),
    ReplacePreservingUndo(String),
    FastToggle,
    CycleRole,
    CycleRoleGroup,
    History(isize),
    Undo,
    Redo,
    RawEvent(RawEvent),
}

enum PromptActionOutcome {
    BufferChanged,
    Continue,
    Return(Event),
}

impl PromptShellAction {
    // Keep this action list, built-in.cli-bindings.yaml, and
    // docs/cli-keybindings.md in sync.
    fn parse(action: &str) -> Option<Self> {
        match action {
            "fast-toggle" => return Some(Self::FastToggle),
            "cycle-role" => return Some(Self::CycleRole),
            "cycle-role-group" => return Some(Self::CycleRoleGroup),
            "prompt-next" => return Some(Self::PromptNext),
            "prompt-previous" => return Some(Self::PromptPrevious),
            "prompt-undo" => return Some(Self::PromptUndo),
            "prompt-redo" => return Some(Self::PromptRedo),
            "submit-prompt" => return Some(Self::SubmitPrompt),
            "insert-newline" => return Some(Self::InsertNewline),
            _ => {}
        }
        let mut parts = action.splitn(3, ':');
        let name = parts.next()?;
        let mode = parts.next()?;
        let command = parts.next()?.to_owned();
        let command = PromptShellCommand {
            command,
            trim: mode == "trim",
        };
        match name {
            "shell-prompt-insert" => Some(Self::Insert(command)),
            "shell-prompt-edit" => Some(Self::Edit(command)),
            "prompt-history-search" => Some(Self::HistorySearch(command)),
            _ => None,
        }
    }
}

fn run_prompt_shell_action(
    term: &tau_cli_term_raw::Term,
    handle: &TermHandle,
    editor_context: Arc<Mutex<EditorContext>>,
    external_editor: Option<&str>,
    prompt_history: &[String],
    action: PromptShellAction,
) -> Result<Option<PromptShellResult>, String> {
    let shell = match &action {
        PromptShellAction::PromptNext => return Ok(Some(PromptShellResult::History(1))),
        PromptShellAction::PromptPrevious => return Ok(Some(PromptShellResult::History(-1))),
        PromptShellAction::PromptUndo => return Ok(Some(PromptShellResult::Undo)),
        PromptShellAction::PromptRedo => return Ok(Some(PromptShellResult::Redo)),
        PromptShellAction::FastToggle => return Ok(Some(PromptShellResult::FastToggle)),
        PromptShellAction::CycleRole => return Ok(Some(PromptShellResult::CycleRole)),
        PromptShellAction::CycleRoleGroup => {
            return Ok(Some(PromptShellResult::CycleRoleGroup));
        }
        PromptShellAction::SubmitPrompt => {
            return Ok(Some(PromptShellResult::RawEvent(
                term.trigger_submit_or_accept_completion(),
            )));
        }
        PromptShellAction::InsertNewline => {
            return Ok(Some(PromptShellResult::RawEvent(
                term.trigger_insert_newline(),
            )));
        }
        PromptShellAction::Insert(shell)
        | PromptShellAction::Edit(shell)
        | PromptShellAction::HistorySearch(shell) => shell,
    };
    let current = trim_prompt_newlines(&handle.get_buffer()).to_owned();
    let cursor = handle.get_cursor();
    let tmp = tempfile::Builder::new()
        .prefix("tau-prompt-")
        .suffix(".tau.md")
        .tempfile()
        .map_err(|e| format!("could not create tempfile: {e}"))?;
    let file_text = match &action {
        PromptShellAction::Edit(_) => append_prompt_trailer(&current, &editor_context),
        PromptShellAction::Insert(_) | PromptShellAction::HistorySearch(_) => current.clone(),
        PromptShellAction::FastToggle
        | PromptShellAction::CycleRole
        | PromptShellAction::CycleRoleGroup
        | PromptShellAction::PromptNext
        | PromptShellAction::PromptPrevious
        | PromptShellAction::PromptUndo
        | PromptShellAction::PromptRedo
        | PromptShellAction::SubmitPrompt
        | PromptShellAction::InsertNewline => unreachable!(),
    };
    std::fs::write(tmp.path(), file_text.as_bytes())
        .map_err(|e| format!("could not write tempfile: {e}"))?;

    let history_picker = match &action {
        PromptShellAction::HistorySearch(_) => {
            let rows = prompt_history_search_rows(prompt_history);
            if rows.is_empty() {
                return Ok(None);
            }
            let prompt_dir = prompt_history_preview_dir(prompt_history)?;
            term.record_prompt_undo();
            Some((rows, prompt_dir))
        }
        _ => None,
    };

    let command = shell.command.as_str();
    tracing::trace!(
        target: "tau_cli::input",
        command,
        prompt_path = %tmp.path().display(),
        cursor,
        "spawning prompt shell action"
    );
    if command.trim().is_empty() {
        return Err("empty shell command".to_owned());
    }

    term.pause_for_external()
        .map_err(|e| format!("could not release terminal: {e}"))?;
    // RAII so a spawn error / panic still restores raw mode.
    struct ResumeGuard<'a>(&'a tau_cli_term_raw::Term);
    impl Drop for ResumeGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.resume_after_external();
        }
    }
    let _guard = ResumeGuard(term);

    let mut command_builder = std::process::Command::new("sh");
    command_builder
        .arg("-c")
        .arg(command)
        .env("TAU_PROMPT_PATH", tmp.path())
        .env("TAU_PROMPT_COLUMN", (cursor + 1).to_string())
        .env("TAU_PROMPT_ROW", "1")
        .env("TAU_EDITOR", external_editor.unwrap_or(""));
    if let Some((_, prompt_dir)) = &history_picker {
        command_builder.env("TAU_PROMPT_HISTORY_DIR", prompt_dir.path());
    }
    command_builder.stdin(if history_picker.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    let mut child = command_builder
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not spawn shell: {e}"))?;
    if let Some((input, _prompt_dir)) = &history_picker
        && let Some(mut stdin) = child.stdin.take()
    {
        match stdin.write_all(input.as_bytes()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            Err(error) => return Err(format!("could not write prompt history to shell: {error}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("could not wait for shell: {e}"))?;
    if !output.status.success() {
        return Ok(None);
    }

    match action {
        PromptShellAction::Insert(_) => {
            let text = String::from_utf8(output.stdout)
                .map_err(|e| format!("command output was not utf-8: {e}"))?;
            let text = if shell.trim {
                text.trim().to_owned()
            } else {
                text
            };
            Ok(Some(PromptShellResult::Insert(text)))
        }
        PromptShellAction::Edit(_) => {
            let new_text = std::fs::read_to_string(tmp.path())
                .map_err(|e| format!("could not read tempfile: {e}"))?;
            let new_text = strip_prompt_trailer(&new_text);
            let new_text = trim_prompt_newlines(new_text).to_owned();
            Ok(Some(PromptShellResult::Replace(new_text)))
        }
        PromptShellAction::HistorySearch(_) => {
            let selected = String::from_utf8(output.stdout)
                .map_err(|e| format!("command output was not utf-8: {e}"))?;
            let selected = if shell.trim {
                selected.trim().to_owned()
            } else {
                selected
            };
            let selected_index = selected.split('\t').next().unwrap_or("").trim();
            if selected_index.is_empty() {
                return Ok(None);
            }
            let index = selected_index
                .parse::<usize>()
                .map_err(|e| format!("history selection was not an index: {e}"))?;
            let text = prompt_history
                .get(index)
                .ok_or_else(|| format!("history selection index {index} is out of range"))?
                .clone();
            Ok(Some(PromptShellResult::ReplacePreservingUndo(text)))
        }
        PromptShellAction::FastToggle
        | PromptShellAction::CycleRole
        | PromptShellAction::CycleRoleGroup
        | PromptShellAction::PromptNext
        | PromptShellAction::PromptPrevious
        | PromptShellAction::PromptUndo
        | PromptShellAction::PromptRedo
        | PromptShellAction::SubmitPrompt
        | PromptShellAction::InsertNewline => unreachable!(),
    }
}

fn prompt_history_search_rows(prompt_history: &[String]) -> String {
    let mut rows = String::new();
    for (index, prompt) in prompt_history.iter().enumerate().rev() {
        if prompt.is_empty() {
            continue;
        }
        rows.push_str(&index.to_string());
        rows.push('\t');
        rows.push_str(&prompt_history_summary(prompt));
        rows.push('\n');
    }
    rows
}

fn prompt_history_preview_dir(prompt_history: &[String]) -> Result<tempfile::TempDir, String> {
    let dir = tempfile::Builder::new()
        .prefix("tau-prompt-history-")
        .tempdir()
        .map_err(|e| format!("could not create prompt history tempdir: {e}"))?;
    for (index, prompt) in prompt_history.iter().enumerate() {
        if prompt.is_empty() {
            continue;
        }
        std::fs::write(dir.path().join(index.to_string()), prompt.as_bytes())
            .map_err(|e| format!("could not write prompt history preview {index}: {e}"))?;
    }
    Ok(dir)
}

fn prompt_history_summary(prompt: &str) -> String {
    prompt.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn append_prompt_trailer(current: &str, editor_context: &Arc<Mutex<EditorContext>>) -> String {
    let context = editor_context
        .lock()
        .expect("editor context mutex poisoned")
        .clone();
    if context.active_prompt.is_none()
        && context.last_agent_response.is_none()
        && context.previous_prompt.is_none()
    {
        return current.to_owned();
    }

    let mut out = trim_prompt_newlines(current).to_owned();
    out.push_str("\n\n");
    out.push_str(PROMPT_TRAILER_MARKER);
    out.push('\n');
    if let Some(text) = context.active_prompt.as_deref().filter(|t| !t.is_empty()) {
        out.push_str("\n## Current response in progress\n\n");
        push_markdown_quote(&mut out, text);
    }
    if let Some(text) = context
        .last_agent_response
        .as_deref()
        .filter(|t| !t.is_empty())
    {
        out.push_str("\n## Last agent response\n\n");
        push_markdown_quote(&mut out, text);
    }
    if let Some(text) = context.previous_prompt.as_deref().filter(|t| !t.is_empty()) {
        out.push_str("\n## Previous prompt\n\n");
        push_markdown_quote(&mut out, text);
    }
    out
}

fn trim_prompt_newlines(text: &str) -> &str {
    text.trim_matches(['\n', '\r'])
}

fn strip_prompt_trailer(text: &str) -> &str {
    let Some((before, _)) = text.split_once(PROMPT_TRAILER_MARKER) else {
        return text;
    };
    before
        .strip_suffix("\n\n")
        .or_else(|| before.strip_suffix('\n'))
        .unwrap_or(before)
}

fn push_markdown_quote(out: &mut String, text: &str) {
    for line in text.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
}

/// Resolves the external editor once at startup: `$EDITOR`, then
/// `$VISUAL`, then the first of `vim`/`vi`/`nano` found on `$PATH`.
fn resolve_external_editor() -> Option<String> {
    for var in ["EDITOR", "VISUAL"] {
        if let Some(val) = std::env::var_os(var) {
            let s = val.to_string_lossy();
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    ["hx", "vim", "vi", "nano"]
        .into_iter()
        .find(|cand| which::which(cand).is_ok())
        .map(str::to_owned)
}
