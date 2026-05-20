//! Filesystem and shell tool extension.
//!
//! Provides `read`, `write`, `edit`, `apply_patch`, `grep`, `find`,
//! `ls`, `shell`, and `gpt_shell` tools.
//!
//! The `echo` tool is available under `cfg(test)` or the
//! `echo-agent` cargo feature for harness-side echo-agent tests.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::{Arc, mpsc};

use tau_proto::{
    Ack, ConfigError, Event, ExtPromptFragmentPublish, ExtSessionContextPublish, Frame,
    FrameReader, FrameWriter, LogEventId, Message, PromptContent, PromptFragment, PromptPriority,
    SessionContextKey, SessionContextValue, SessionStarted, ToolExecutionMode, ToolSpec,
};

mod agents;
mod argument;
mod config;
mod diff;
mod display;
mod isolation;
mod semaphore;
mod tools;
mod truncate;

#[cfg(test)]
mod tests;

use crate::agents::{ancestor_dirs, discover_session_agents_files};
use crate::config::{ExtConfig, ShellConfig};
use crate::semaphore::Semaphore;
#[cfg(any(test, feature = "echo-agent"))]
use crate::tools::ECHO_TOOL_NAME;
use crate::tools::{
    APPLY_PATCH_TOOL_NAME, EDIT_TOOL_NAME, FIND_TOOL_NAME, GPT_SHELL_TOOL_NAME, GREP_TOOL_NAME,
    LS_TOOL_NAME, READ_TOOL_NAME, SHELL_TOOL_NAME, WRITE_TOOL_NAME, execute_tool,
};

/// Runs the extension on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run_impl(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// The test-only `echo` tool is registered when built with
/// `cfg(test)` or the `echo-agent` cargo feature.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_impl(reader, writer)
}

fn run_impl<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    #[cfg(any(test, feature = "echo-agent"))]
    let echo_tool = Some(ToolSpec {
        name: tau_proto::ToolName::new(ECHO_TOOL_NAME),
        model_visible_name: None,
        description: Some("Echo the provided payload unchanged".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    });
    #[cfg(not(any(test, feature = "echo-agent")))]
    let echo_tool: Option<ToolSpec> = None;
    let tools = echo_tool.into_iter().chain([
        ToolSpec {
            name: tau_proto::ToolName::new(READ_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Reads a file. Defaults to reading the whole file in one call — \
                 output is capped at 2000 lines / 50 KB. Truncated output keeps \
                 the first 1000 and last 1000 lines separated by a literal `...` line. \
                 Prefer one full read. Pass `start_line`/`line_count` only to \
                 fetch a specific known slice of a file you already know is large. \
                 Returned content lines are prefixed by their 1-based line number and a space; \
                 CRLF, CR, and missing final line endings are marked after the number, e.g. \
                 `2(crlf)`, `3(cr)`, or `4(no_nl)`. Invalid UTF-8 and lines that would exceed \
                 the 50 KB output budget are marker-only, e.g. `1(invalid-utf8)` or \
                 `1(truncated)`. Truncated results include `truncated: true`, `total_lines`, \
                 and `total_bytes`; `valid_utf8: false` is included only when applicable."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file"
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Optional, 1-based. Omit to start at line 1 (the default)."
                    },
                    "line_count": {
                        "type": "integer",
                        "description": "Optional. Omit to read to end of file (the default and preferred mode). Set this only to continue past a previous truncation, or to fetch a known specific slice of a large file — do NOT pre-slice an ordinary file you haven't already established is large."
                    }
                },
                "required": ["path"]
            })),
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(WRITE_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Write content to a file, creating it and any missing parent directories \
                 if they do not exist. Follows symlinks and overwrites the symlink target \
                 instead of replacing the symlink. Returns the path, bytes written, whether \
                 the requested path was created, whether filesystem contents changed, and \
                 symlink target metadata when the requested path was a symlink."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file"
                    },
                    "content": {
                        "type": "string",
                        "description": "File contents, written verbatim. Embed real newlines directly — do NOT use backslash-n escape sequences."
                    }
                },
                "required": ["path", "content"]
            })),
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Exclusive,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Edit a file using exact text replacement. Each edit is matched against \
                 the original file, optionally restricted to start_line and line_count, \
                 and replaces the first matches in that range up to max_matches, capped \
                 at 100 replacements per call. Replacement ranges from all edits must \
                 not overlap. Returns the path, the number of replacements, and optional \
                 unified diff-like output summarizing the change against the previous contents."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file"
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements matched against the original file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {
                                    "type": "string",
                                    "description": "Exact text to find, matched verbatim. Embed real newlines directly — do NOT use backslash-n escape sequences."
                                },
                                "newText": {
                                    "type": "string",
                                    "description": "Replacement text, written verbatim. Embed real newlines directly — do NOT use backslash-n escape sequences."
                                },
                                "max_matches": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Maximum number of matches to replace for this edit. Defaults to 1. Matches are replaced from the start of the selected range."
                                },
                                "start_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Optional 1-based inclusive start line for searching this edit. Defaults to line 1."
                                },
                                "line_count": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Optional number of lines to search starting at start_line. Defaults to the rest of the file."
                                }
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "required": ["path", "edits"]
            })),
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Exclusive,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Custom,
            parameters: None,
            format: Some(tau_proto::ToolFormat::Grammar {
                syntax: tau_proto::ToolGrammarSyntax::Lark,
                definition: crate::tools::apply_patch::APPLY_PATCH_LARK_GRAMMAR.to_owned(),
            }),
            enabled_by_default: false,
            execution_mode: ToolExecutionMode::Exclusive,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(GREP_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Search file contents for a pattern using ripgrep. Patterns are literal by default; \
                 regex metacharacters like `|` require `regex: true`. Returns matching lines \
                 with file paths and line numbers. Respects .gitignore. Output is truncated at \
                 `limit` matches or 50KB. Long lines are truncated to 500 chars."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern. Treated as a literal string by default. Set `regex: true` to interpret as a regex."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search (default: current directory)"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.rs'"
                    },
                    "ignoreCase": {
                        "type": "boolean",
                        "description": "Case-insensitive search (default: false)"
                    },
                    "regex": {
                        "type": "boolean",
                        "description": "Interpret `pattern` as a regex instead of a literal string (default: false)"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Number of lines to show before and after each match (default: 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of matches to return (default: 100)"
                    }
                },
                "required": ["pattern"]
            })),
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(FIND_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Search for files by glob pattern. Returns only file paths (directories are \
                 never included, even with '**/*') relative to the search directory. Respects \
                 .gitignore. Output is truncated at `limit` results or 50KB. Use the ls tool \
                 if you want to see directory entries."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern matched against file paths relative to `path`. `**` matches any number of intermediate directories, including zero — so `**/*.rs` finds both top-level `a.rs` and nested `src/a.rs`. Directories are not returned, even with `**/*`."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search (default: current directory)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 1000)"
                    }
                },
                "required": ["pattern"]
            })),
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(LS_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "List directory contents. Returns entries sorted alphabetically, with '/' suffix \
                 for directories. Includes dotfiles. Output is truncated at `limit` entries or 50KB."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list (default: current directory)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of entries to return (default: 500)"
                    }
                }
            })),
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Execute a shell command via `sh -c`. Non-zero exits and timeouts \
                 are tool errors with output details. Output is capped at 2000 lines / \
                 50 KB; truncated output keeps the first 1000 and last 1000 lines \
                 separated by a literal `...` line. Output lines are prefixed with `out ` \
                 for stdout or `err ` for stderr; missing trailing newlines are marked, e.g. \
                 `out(no_nl)`. Invalid UTF-8 and lines that would exceed the 50 KB output \
                 budget are marker-only, e.g. `out(invalid-utf8)` or `err(truncated)`. \
                 Truncated results include `truncated: true`, `total_lines`, and `total_bytes`. \
                 Commands taking longer than 5 seconds include duration metadata. Prefer dedicated \
                 tools like `read`, `grep`, and `find` when they fit."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Timeout in seconds. The command is killed if it exceeds this. Default: 120"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the command"
                    }
                },
                "required": ["command"]
            })),
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Exclusive,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(GPT_SHELL_TOOL_NAME),
            model_visible_name: Some(tau_proto::ToolName::new("shell_command")),
            description: Some(
                "Run a shell command. Non-zero exits and timeouts are returned as structured \
                 command results with output details. Output is capped at 2000 lines / 50 KB; \
                 truncated output keeps the first 1000 and last 1000 lines separated by `...`. \
                 Output lines are prefixed with `out ` for stdout or `err ` for stderr; missing \
                 trailing newlines are marked with `(no_nl)`. Invalid UTF-8 and lines that \
                 would exceed the 50 KB output budget are marker-only. Truncated results \
                 include `truncated: true`, `total_lines`, and `total_bytes`. Commands taking \
                 longer than 5 seconds include approximate duration metadata. For file changes, \
                 prefer apply_patch."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Timeout in seconds. The command is killed if it exceeds this. Default: 120"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the command"
                    }
                },
                "required": ["command"]
            })),
            format: None,
            enabled_by_default: false,
            execution_mode: ToolExecutionMode::Exclusive,
            background_support: None,
        },
    ]);

    let mut handshake = tau_extension::Handshake::tool("tau-ext-shell").subscribe([
        tau_proto::EventName::TOOL_INVOKE,
        tau_proto::EventName::SESSION_STARTED,
        tau_proto::EventName::UI_SHELL_COMMAND,
    ]);
    for tool in tools {
        handshake = handshake.register_tool(tool);
    }
    handshake
        .announce_event(Event::ExtPromptFragmentPublish(ExtPromptFragmentPublish {
            fragment: shell_cwd_prompt_fragment(),
        }))
        .ready_message("filesystem and shell tools ready")
        .run(&mut writer)?;

    // Response channel: worker threads send frames here, writer thread
    // drains them onto the wire.
    let (tx, rx) = mpsc::channel::<Frame>();
    let sem = Arc::new(Semaphore::new(16));

    // Writer thread: drains response frames and writes them to the wire.
    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        for frame in rx {
            writer
                .write_frame(&frame)
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
            writer
                .flush()
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
        }
        Ok(())
    });

    let mut config = ExtConfig::default();

    // Reader loop: dispatch each tool invocation to a worker thread.
    //
    // ToolInvoke is sent point-to-point (not via the harness event log)
    // so it carries no `LogEventId` and needs no ack — the
    // ToolResult/ToolError correlated by call_id is the implicit reply.
    //
    // Other subscribed events (SessionStarted) come wrapped as
    // `Message::LogEvent` and require an `Ack` after processing.
    while let Some(frame) = reader.read_frame()? {
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Message(Message::Configure(msg)) => {
                match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                    Ok(cfg) => config = cfg,
                    Err(message) => {
                        tx.send(Frame::Message(Message::ConfigError(ConfigError {
                            message,
                        })))?;
                    }
                }
            }
            Frame::Event(Event::ToolInvoke(invoke)) => {
                // Block here until a permit is free. This bounds the
                // total number of in-flight worker threads — without
                // it, a burst of ToolInvokes would spawn unbounded
                // native threads that then serialize on the semaphore.
                let permit = sem.acquire();
                let tx = tx.clone();
                let shell_config = config.shell.clone();
                std::thread::spawn(move || {
                    let _permit = permit;
                    dispatch_tool_invoke(invoke, shell_config, &tx);
                });
            }
            Frame::Event(Event::SessionStarted(started)) => {
                dispatch_session_started(started, &tx);
            }
            Frame::Event(Event::UiShellCommand(cmd)) => {
                // User-initiated `!`/`!!` — run on a worker thread
                // and stream chunks out via the same tx writer.
                let permit = sem.acquire();
                let tx = tx.clone();
                let shell_config = config.shell.clone();
                std::thread::spawn(move || {
                    let _permit = permit;
                    crate::tools::shell::dispatch_user_shell_command(cmd, shell_config, &tx);
                });
            }
            Frame::Message(Message::Disconnect(_)) => break,
            _ => {}
        }
        if let Some(id) = log_id {
            ack_log_event(id, &tx);
        }
    }

    // Drop the sender so the writer thread exits.
    drop(tx);
    writer_handle
        .join()
        .map_err(|_| "writer thread panicked")?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
}

/// Execute a single tool invocation and send the response event(s).
fn dispatch_tool_invoke(
    invoke: tau_proto::ToolInvoke,
    shell_config: ShellConfig,
    tx: &mpsc::Sender<Frame>,
) {
    let events = execute_tool(invoke, &shell_config);
    for event in events {
        let _ = tx.send(Frame::Event(event));
    }
}

fn dispatch_session_started(started: SessionStarted, tx: &mpsc::Sender<Frame>) {
    for event in build_session_started_events(started) {
        let _ = tx.send(Frame::Event(event));
    }
}

fn ack_log_event(id: LogEventId, tx: &mpsc::Sender<Frame>) {
    let _ = tx.send(Frame::Message(Message::Ack(Ack { up_to: id })));
}

fn build_session_started_events(started: SessionStarted) -> Vec<Event> {
    let mut events = Vec::new();

    if let Ok(cwd) = std::env::current_dir() {
        events.push(Event::ExtSessionContextPublish(ExtSessionContextPublish {
            session_id: started.session_id.clone(),
            key: SessionContextKey::new("cwd"),
            value: SessionContextValue(serde_json::Value::String(cwd.display().to_string())),
        }));
    }

    let skill_dirs = session_skill_dirs(std::env::current_dir().ok(), dirs::home_dir());

    let result = tau_skills::load_skills_from_skill_dirs(&skill_dirs);
    push_skill_diagnostic_events(&mut events, result.diagnostics);
    for skill in result.skills {
        let file_path = skill.file_path.canonicalize().unwrap_or(skill.file_path);
        events.push(Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: skill.name.into(),
            description: skill.description,
            file_path,
            add_to_prompt: skill.add_to_prompt,
        }));
    }

    for agents_file in discover_session_agents_files() {
        events.push(Event::ExtAgentsMdAvailable(
            tau_proto::ExtAgentsMdAvailable {
                file_path: agents_file.file_path,
                content: agents_file.content,
            },
        ));
    }

    events.push(Event::ExtensionContextReady(
        tau_proto::ExtensionContextReady {
            session_id: started.session_id,
        },
    ));
    events
}

fn shell_cwd_prompt_fragment() -> PromptFragment {
    PromptFragment::new(
        "shell.cwd",
        PromptPriority::new(900),
        PromptContent::new(
            "{{#each session_context.cwd}}{{#if @first}}Current working directory: \
             {{value}}{{/if}}{{/each}}",
        ),
    )
}

fn push_skill_diagnostic_events(
    events: &mut Vec<Event>,
    diagnostics: Vec<tau_skills::SkillDiagnostic>,
) {
    for diagnostic in diagnostics {
        let (kind, level) = match diagnostic.kind {
            tau_skills::DiagnosticKind::Warning => ("warning", tau_proto::HarnessInfoLevel::Normal),
            tau_skills::DiagnosticKind::Collision => {
                ("collision", tau_proto::HarnessInfoLevel::Important)
            }
            tau_skills::DiagnosticKind::Skipped => {
                ("skipped", tau_proto::HarnessInfoLevel::Important)
            }
        };
        events.push(Event::HarnessInfo(tau_proto::HarnessInfo {
            message: format!(
                "skill {kind}: {}\n{}",
                diagnostic.path.display(),
                diagnostic.message
            ),
            level,
        }));
    }
}

fn session_skill_dirs(
    cwd: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Vec<tau_skills::SkillDir> {
    let mut skill_dirs = Vec::new();
    if let Some(cwd) = cwd.as_deref() {
        for project_dir in project_skill_ancestor_dirs(cwd, home.as_deref()) {
            push_existing_project_skill_dir(
                &mut skill_dirs,
                project_dir.join(".agents").join("skills"),
            );
            push_existing_project_skill_dir(
                &mut skill_dirs,
                project_dir.join(".agents.local").join("skills"),
            );
        }
    }
    if let Some(home) = home {
        skill_dirs.push(user_skill_dir(home.join(".agents").join("skills")));
        skill_dirs.push(user_skill_dir(home.join(".agents.local").join("skills")));
        skill_dirs.push(user_skill_dir(
            home.join(".config").join("agents").join("skills"),
        ));
        skill_dirs.push(user_skill_dir(
            home.join(".config").join("agents.local").join("skills"),
        ));
    }
    skill_dirs
}

fn project_skill_ancestor_dirs(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    ancestor_dirs(cwd)
        .into_iter()
        .filter(|dir| dir.parent().is_some())
        .filter(|dir| {
            let Some(home) = home else {
                return true;
            };
            !cwd.starts_with(home) || (dir.starts_with(home) && dir != home)
        })
        .collect()
}

fn push_existing_project_skill_dir(
    skill_dirs: &mut Vec<tau_skills::SkillDir>,
    path: std::path::PathBuf,
) {
    if path.is_dir() {
        skill_dirs.push(project_skill_dir(path));
    }
}

fn project_skill_dir(path: std::path::PathBuf) -> tau_skills::SkillDir {
    tau_skills::SkillDir {
        path,
        add_to_prompt_by_default: true,
    }
}

fn user_skill_dir(path: std::path::PathBuf) -> tau_skills::SkillDir {
    tau_skills::SkillDir {
        path,
        add_to_prompt_by_default: false,
    }
}
