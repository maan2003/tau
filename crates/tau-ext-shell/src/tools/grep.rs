//! `grep` tool: ripgrep-backed search using `rg --json`.

use std::fmt;
use std::io::{BufReader, Read};
use std::process::Command;

use tau_proto::CborValue;

use crate::argument::{
    argument_text, optional_argument_bool, optional_argument_int, optional_argument_text,
};
use crate::display::{ToolFailure, ToolOutput, text_stats};
use crate::isolation::apply_command_isolation;
use crate::truncate::{truncate_head, truncate_line};

pub(crate) const DEFAULT_GREP_LIMIT: usize = 100;
pub(crate) const GREP_MAX_LINE_LENGTH: usize = 500;

pub(crate) fn run_grep(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    let pattern = argument_text(arguments, "pattern")?;
    let path = optional_argument_text(arguments, "path");
    let glob = optional_argument_text(arguments, "glob");
    let ignore_case = optional_argument_bool(arguments, "ignoreCase").unwrap_or(false);
    // Literal matching is the default. Most callers are searching for
    // an exact string and regex metacharacters in that string (`[`,
    // `(`, `.`, `?`, `+`, `*`, `|`, `{`, `\`) would otherwise either
    // fail to parse or silently match something unintended. Regex
    // users opt in explicitly with `regex: true`.
    let regex = optional_argument_bool(arguments, "regex").unwrap_or(false);
    let context = optional_argument_int(arguments, "context").map(|v| v.max(0) as usize);
    let limit = optional_argument_int(arguments, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(DEFAULT_GREP_LIMIT);

    let search_path = path.as_deref().unwrap_or(".");

    // Use `--json` for structured output. This replaces the previous
    // hand-rolled `PATH:LINE:CONTENT` vs `PATH-LINE-CONTENT` line
    // classifier, which had a known misclassification mode on paths
    // like `file-12-34.txt`. The JSON envelope cleanly separates
    // match from context records.
    //
    // `--with-filename` is still needed to keep the path field
    // present when searching a single file, so the rendered output
    // continues to lead with `path:` even in that case.
    let mut args: Vec<String> = vec![
        "--json".to_owned(),
        "--hidden".to_owned(),
        "--with-filename".to_owned(),
    ];
    if ignore_case {
        args.push("--ignore-case".to_owned());
    }
    if !regex {
        args.push("--fixed-strings".to_owned());
    }
    if let Some(ref g) = glob {
        args.push("--glob".to_owned());
        args.push(g.clone());
    }
    if let Some(ctx) = context {
        args.push(format!("--context={ctx}"));
    }
    args.push("--".to_owned());
    args.push(pattern.clone());
    args.push(search_path.to_owned());

    let display_args = match glob.as_deref() {
        Some(g) => format!("{pattern:?} in {search_path} [{g}]"),
        None => format!("{pattern:?} in {search_path}"),
    };
    let with_args = |f: ToolFailure| f.with_args(display_args.clone());

    let mut cmd = Command::new("rg");
    cmd.args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_command_isolation(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| with_args(ToolFailure::from(format!("failed to start ripgrep: {e}"))))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| with_args(ToolFailure::from("ripgrep stdout pipe missing".to_owned())))?;
    let GrepStreamResult {
        result_lines,
        match_count,
        lines_truncated,
        match_limit_reached,
    } = read_grep_json(stdout, limit);

    // If the limit fired we may have killed reading mid-stream; make
    // sure the child does not linger.
    if match_limit_reached {
        let _ = child.kill();
    }

    let output = child.wait_with_output().map_err(|e| {
        with_args(ToolFailure::from(format!(
            "failed to wait for ripgrep: {e}"
        )))
    })?;

    // rg exit codes: 0=matches found, 1=no matches, 2=error.
    // Exit-2 is overloaded — ripgrep emits regex parse errors, IO
    // errors, and permission denials all under the same code. Classify
    // the stderr into a short, single-line message so the UI doesn't
    // surface a multi-line regex-parser dump in the inline tool block.
    let status = output.status.code();
    if status == Some(2) {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        return Err(with_args(ToolFailure::from(
            classify_ripgrep_stderr(stderr_raw.trim()).to_string(),
        )));
    }

    if result_lines.is_empty() {
        let mut display = crate::display::ok_display(display_args.clone());
        display.status_text = "ok: no matches".to_owned();
        display.stats.matches = Some(0);
        return Ok(ToolOutput {
            result: grep_result_map(
                &pattern,
                search_path,
                glob.as_deref(),
                status,
                0,
                "no matches found".to_owned(),
            ),
            display,
        });
    }

    let mut output_text = result_lines.join("\n");

    // Apply byte-level truncation to the assembled output.
    let byte_truncated = truncate_head(&output_text);
    if byte_truncated.was_truncated {
        output_text = byte_truncated.content;
    }

    // Build notices.
    let mut notices = Vec::new();
    if match_limit_reached {
        notices.push(format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern.",
            limit * 2
        ));
    }
    if byte_truncated.was_truncated {
        notices.push("50KB output limit reached.".to_owned());
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines."
        ));
    }

    if !notices.is_empty() {
        output_text.push_str("\n\n[");
        output_text.push_str(&notices.join(" "));
        output_text.push(']');
    }

    let mut display = crate::display::ok_display(display_args);
    display.stats = text_stats(&output_text);
    display.stats.matches = Some(match_count as u64);
    Ok(ToolOutput {
        result: grep_result_map(
            &pattern,
            search_path,
            glob.as_deref(),
            status,
            match_count,
            output_text,
        ),
        display,
    })
}

/// Categorized ripgrep failure (exit code 2). The variants encode the
/// kind of fault; the `Display` impl produces the short single-line
/// message we surface as the tool error. Untagged callers stringify
/// this via `to_string()`. When the unified tool-usage descriptor
/// lands, the variants can be mapped to its `status` field directly
/// instead of being flattened to a string.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RipgrepError {
    /// Bad regex / pattern from the agent. Carries ripgrep's trailing
    /// `error: <diagnostic>` line (e.g. `unclosed group`) when found.
    Usage {
        detail: String,
    },
    NotFound,
    Permission,
    /// Anything else. Carries the first non-empty stderr line so the
    /// chip stays readable but we don't lose the signal entirely.
    Runtime {
        detail: String,
    },
}

impl fmt::Display for RipgrepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage { detail } if !detail.is_empty() => {
                write!(f, "regex parse error: {detail}")
            }
            Self::Usage { .. } => f.write_str("regex parse error"),
            Self::NotFound => f.write_str("no such file or directory"),
            Self::Permission => f.write_str("permission denied"),
            Self::Runtime { detail } if !detail.is_empty() => {
                write!(f, "ripgrep error: {detail}")
            }
            Self::Runtime { .. } => f.write_str("ripgrep error"),
        }
    }
}

/// Classify ripgrep's stderr (exit code 2). ripgrep prints stable,
/// well-known prefixes for each failure class — `regex parse error:`
/// for a bad pattern from the agent, and the OS-error suffix
/// (`(os error 2)` / `(os error 13)`) for not-found and
/// permission-denied — so we can label these without parsing
/// arbitrary downstream text.
pub(crate) fn classify_ripgrep_stderr(stderr: &str) -> RipgrepError {
    if stderr.contains("regex parse error")
        || stderr.contains("error parsing regex")
        || stderr.contains("unrecognized escape sequence")
    {
        // ripgrep's regex-parser output puts the human-readable
        // diagnostic on a trailing `error: <text>` line; the header
        // and pattern/caret lines aren't useful for a one-line chip.
        let detail = stderr
            .lines()
            .filter_map(|l| l.trim().strip_prefix("error:"))
            .map(str::trim)
            .next_back()
            .unwrap_or("")
            .to_owned();
        return RipgrepError::Usage { detail };
    }
    if stderr.contains("(os error 2)") || stderr.contains("No such file or directory") {
        return RipgrepError::NotFound;
    }
    if stderr.contains("(os error 13)") || stderr.contains("Permission denied") {
        return RipgrepError::Permission;
    }
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned();
    RipgrepError::Runtime { detail }
}

/// Result of streaming and rendering rg's `--json` output.
struct GrepStreamResult {
    result_lines: Vec<String>,
    match_count: usize,
    lines_truncated: bool,
    match_limit_reached: bool,
}

/// Minimal rg `--json` envelope. Only the fields we render are
/// deserialized; everything else is dropped.
#[derive(serde::Deserialize)]
struct RgRecord {
    #[serde(rename = "type")]
    kind: String,
    data: RgData,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RgData {
    path: Option<RgText>,
    lines: Option<RgText>,
    line_number: Option<u64>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RgText {
    text: Option<String>,
}

/// Stream rg's JSON Lines output, build the legacy
/// `PATH:LINE:CONTENT` / `PATH-LINE-CONTENT` rendering, and break
/// early once the match limit is reached.
fn read_grep_json<R: Read>(stdout: R, limit: usize) -> GrepStreamResult {
    use std::io::BufRead as _;
    let reader = BufReader::new(stdout);
    let mut result_lines = Vec::new();
    let mut match_count = 0usize;
    let mut lines_truncated = false;
    let mut match_limit_reached = false;
    let mut current_path: Option<String> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<RgRecord>(&line) else {
            continue;
        };
        match record.kind.as_str() {
            "begin" => {
                current_path = record.data.path.and_then(|p| p.text);
            }
            "match" | "context" => {
                let path = record
                    .data
                    .path
                    .as_ref()
                    .and_then(|p| p.text.as_deref())
                    .or(current_path.as_deref())
                    .unwrap_or("");
                let lineno = record.data.line_number.unwrap_or(0);
                let text = record.data.lines.and_then(|l| l.text).unwrap_or_default();
                let text = strip_eol(&text);
                let is_match = record.kind == "match";
                if is_match {
                    match_count += 1;
                    if limit < match_count {
                        match_limit_reached = true;
                        break;
                    }
                }
                let sep = if is_match { ':' } else { '-' };
                let rendered = format!("{path}{sep}{lineno}{sep}{text}");
                if rendered.len() > GREP_MAX_LINE_LENGTH {
                    result_lines.push(truncate_line(&rendered, GREP_MAX_LINE_LENGTH));
                    lines_truncated = true;
                } else {
                    result_lines.push(rendered);
                }
            }
            _ => {}
        }
    }

    GrepStreamResult {
        result_lines,
        match_count,
        lines_truncated,
        match_limit_reached,
    }
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

/// Build the CBOR result map for `grep`. Echoes `pattern`/`path`/`glob`
/// alongside the match count and output so UI renderers have enough
/// context to label the call without re-reading the request arguments.
pub(crate) fn grep_result_map(
    pattern: &str,
    search_path: &str,
    glob: Option<&str>,
    status: Option<i32>,
    matches: usize,
    output_text: String,
) -> CborValue {
    let mut fields = vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text(pattern.to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(search_path.to_owned()),
        ),
        (
            CborValue::Text("status".to_owned()),
            status
                .map(|code| CborValue::Integer((code as i64).into()))
                .unwrap_or(CborValue::Null),
        ),
        (
            CborValue::Text("matches".to_owned()),
            CborValue::Integer((matches as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text.clone()),
        ),
        (
            CborValue::Text("output_lines".to_owned()),
            CborValue::Integer((output_text.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("output_bytes".to_owned()),
            CborValue::Integer((output_text.len() as i64).into()),
        ),
    ];
    if let Some(glob) = glob {
        fields.push((
            CborValue::Text("glob".to_owned()),
            CborValue::Text(glob.to_owned()),
        ));
    }
    CborValue::Map(fields)
}
