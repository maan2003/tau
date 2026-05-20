//! `read` tool: read a file (optionally a line slice).

use std::fs;
use std::path::{Path, PathBuf};

use tau_proto::CborValue;

use crate::argument::{argument_text, optional_argument_int};
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES};

pub(crate) fn read_file(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let start_line_arg = optional_argument_int(arguments, "start_line");
    let line_count_arg = optional_argument_int(arguments, "line_count");
    let start_line = parse_read_start_line(start_line_arg)?;
    let line_count = parse_read_line_count(line_count_arg)?;
    let path_buf = PathBuf::from(&path);
    let display_path = path_buf.display().to_string();
    let range = format_read_range(start_line_arg.map(|_| start_line), line_count);
    let display_args = format!("{display_path} {range}");

    let file_bytes = fs::metadata(&path_buf)
        .map_err(|error| ToolFailure::from(error.to_string()).with_args(display_args.clone()))?
        .len() as usize;
    let sliced = stream_slice_lines(&path_buf, start_line, line_count)
        .map_err(|error| ToolFailure::from(error.to_string()).with_args(display_args.clone()))?;
    if sliced.total_lines != 0 && start_line > sliced.total_lines {
        return Err(ToolFailure::new(format!(
            "start_line {start_line} is past end of file (total_lines: {})",
            sliced.total_lines
        ))
        .with_args(display_args));
    }
    let total_lines = sliced.total_lines;
    let truncated = truncate_read_content(&sliced.content, start_line, total_lines, file_bytes);
    let content_value = CborValue::Text(truncated.content.clone());
    debug_assert!(truncated.line_count <= sliced.line_count);
    let mut entries = vec![
        (
            CborValue::Text("line-numbered content".to_owned()),
            content_value,
        ),
        (
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((total_lines as i64).into()),
        ),
    ];
    if !sliced.valid_utf8 {
        entries.push((
            CborValue::Text("valid_utf8".to_owned()),
            CborValue::Bool(false),
        ));
    }
    if truncated.was_truncated {
        entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
    }
    let mut display = ok_display(display_args);
    display.stats = text_stats(&truncated.content);
    Ok(ToolOutput {
        result: CborValue::Map(entries),
        display,
    })
}

pub(crate) struct ReadSlice {
    pub(crate) content: String,
    pub(crate) line_count: usize,
    pub(crate) valid_utf8: bool,
    /// Total lines in the source. For [`stream_slice_lines`] this is
    /// computed by scanning the rest of the file after the slice ends.
    pub(crate) total_lines: usize,
}

struct TruncatedRead {
    content: String,
    was_truncated: bool,
    line_count: usize,
}

fn truncate_read_content(
    content: &str,
    start_line: usize,
    total_lines: usize,
    file_bytes: usize,
) -> TruncatedRead {
    let total_rendered_lines = content.lines().count();
    let total_rendered_bytes = content.len();
    let mut was_truncated = false;
    let mut rendered = String::new();
    let mut rendered_bytes = 0usize;
    let mut line_count = 0usize;

    if total_rendered_lines <= MAX_OUTPUT_LINES && total_rendered_bytes <= MAX_OUTPUT_BYTES {
        rendered.push_str(content);
        line_count = total_rendered_lines;
    } else {
        was_truncated = true;
        for (line_index, line) in content.lines().enumerate() {
            if line_count >= MAX_OUTPUT_LINES {
                break;
            }
            let separator_bytes = usize::from(line_index != 0);
            if rendered_bytes + separator_bytes >= MAX_OUTPUT_BYTES {
                break;
            }
            let remaining = MAX_OUTPUT_BYTES - rendered_bytes - separator_bytes;
            if line.len() + separator_bytes <= remaining {
                if line_index != 0 {
                    rendered.push('\n');
                    rendered_bytes += 1;
                }
                rendered.push_str(line);
                rendered_bytes += line.len();
                line_count += 1;
            } else {
                let prefix = utf8_prefix(line, remaining);
                if !prefix.is_empty() {
                    if line_index != 0 {
                        rendered.push('\n');
                    }
                    rendered.push_str(&mark_line_truncated(prefix));
                    line_count += 1;
                }
                break;
            }
        }
    }

    if was_truncated {
        let end_line = if line_count == 0 {
            start_line.saturating_sub(1)
        } else {
            start_line.saturating_add(line_count).saturating_sub(1)
        };
        let continuation = if rendered.contains("(truncated)") {
            "Line was truncated by byte cap; line-based continuation cannot resume within a line."
        } else {
            "Use start_line and line_count to continue reading."
        };
        rendered.push_str(&format!(
            "\n\n[Showing lines {start_line}-{end_line} of {total_lines} ({file_bytes} bytes total). \
             {continuation}]"
        ));
    }

    TruncatedRead {
        content: rendered,
        was_truncated,
        line_count,
    }
}

fn utf8_prefix(input: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(input.len());
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn mark_line_truncated(line: &str) -> String {
    if let Some((line_number, rest)) = line.split_once(' ') {
        format!("{line_number}(truncated) {rest}...")
    } else {
        format!("{line}(truncated) ...")
    }
}

/// Stream `[start_line, start_line+count)` from `path` without
/// slurping the whole file. Continues reading past the slice end only
/// to count remaining lines (so the caller can report `total_lines`
/// for a "showing N of M" hint).
fn stream_slice_lines(
    path: &Path,
    start_line: usize,
    line_count: Option<usize>,
) -> std::io::Result<ReadSlice> {
    let bytes = fs::read(path)?;
    let take = line_count.unwrap_or(usize::MAX);
    let mut state = SliceState::new(start_line, take);

    let mut line_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                let is_crlf = index + 1 < bytes.len() && bytes[index + 1] == b'\n';
                let ending = if is_crlf {
                    LineEndingKind::Crlf
                } else {
                    LineEndingKind::Cr
                };
                state.push_line(&bytes[line_start..index], Some(ending));
                index += if is_crlf { 2 } else { 1 };
                line_start = index;
            }
            b'\n' => {
                state.push_line(&bytes[line_start..index], Some(LineEndingKind::Lf));
                index += 1;
                line_start = index;
            }
            _ => index += 1,
        }
    }

    if line_start < bytes.len() {
        state.push_line(&bytes[line_start..], None);
    }

    Ok(state.finish())
}

struct SliceState {
    lines: Vec<ReadLine>,
    start_line: usize,
    take: usize,
    total_lines: usize,
    lf_count: usize,
    crlf_count: usize,
    cr_count: usize,
    valid_utf8: bool,
}

struct ReadLine {
    number: usize,
    content: Option<String>,
    ending: Option<LineEndingKind>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineEndingKind {
    Lf,
    Crlf,
    Cr,
}

impl SliceState {
    fn new(start_line: usize, take: usize) -> Self {
        Self {
            lines: Vec::new(),
            start_line,
            take,
            total_lines: 0,
            lf_count: 0,
            crlf_count: 0,
            cr_count: 0,
            valid_utf8: true,
        }
    }

    fn push_line(&mut self, line: &[u8], ending: Option<LineEndingKind>) {
        self.total_lines += 1;
        match ending {
            Some(LineEndingKind::Lf) => self.lf_count += 1,
            Some(LineEndingKind::Crlf) => self.crlf_count += 1,
            Some(LineEndingKind::Cr) => self.cr_count += 1,
            None => {}
        }

        let valid_line = std::str::from_utf8(line).ok();
        if valid_line.is_none() {
            self.valid_utf8 = false;
        }
        if self.start_line <= self.total_lines && self.lines.len() < self.take {
            self.lines.push(ReadLine {
                number: self.total_lines,
                content: valid_line.map(ToOwned::to_owned),
                ending,
            });
        }
    }

    fn finish(self) -> ReadSlice {
        let default_ending = self.default_line_ending();
        let content = self
            .lines
            .iter()
            .map(|line| render_read_line(line, default_ending))
            .collect::<Vec<_>>()
            .join("\n");
        ReadSlice {
            content,
            line_count: self.lines.len(),
            valid_utf8: self.valid_utf8,
            total_lines: self.total_lines,
        }
    }

    fn default_line_ending(&self) -> Option<LineEndingKind> {
        let counts = [
            (self.lf_count, LineEndingKind::Lf),
            (self.crlf_count, LineEndingKind::Crlf),
            (self.cr_count, LineEndingKind::Cr),
        ];
        let (max_count, default) = counts.iter().copied().max_by_key(|(count, _)| *count)?;
        if max_count == 0
            || counts
                .iter()
                .filter(|(count, _)| *count == max_count)
                .count()
                != 1
        {
            None
        } else {
            Some(default)
        }
    }
}

fn render_read_line(line: &ReadLine, default_ending: Option<LineEndingKind>) -> String {
    let mut markers = Vec::new();
    if line.content.is_none() {
        markers.push("invalid-utf8");
    }
    if line.ending.is_none() || line.ending != default_ending {
        markers.push(match line.ending {
            Some(LineEndingKind::Lf) => "lf",
            Some(LineEndingKind::Crlf) => "crlf",
            Some(LineEndingKind::Cr) => "cr",
            None => "no_nl",
        });
    }

    let marker = if markers.is_empty() {
        String::new()
    } else {
        format!("({})", markers.join(","))
    };
    match &line.content {
        Some(content) => format!("{}{marker} {content}", line.number),
        None => format!("{}{marker}", line.number),
    }
}

fn parse_read_start_line(value: Option<i64>) -> Result<usize, ToolFailure> {
    match value {
        None => Ok(1),
        Some(value) if value < 1 => Err(ToolFailure::new("start_line must be >= 1")),
        Some(value) => Ok(value as usize),
    }
}

fn parse_read_line_count(value: Option<i64>) -> Result<Option<usize>, ToolFailure> {
    match value {
        None => Ok(None),
        Some(value) if value < 1 => Err(ToolFailure::new("line_count must be >= 1")),
        Some(value) => Ok(Some(value as usize)),
    }
}

pub(crate) fn format_read_range(start_line: Option<usize>, line_count: Option<usize>) -> String {
    match (start_line, line_count) {
        (None, None) => "..".to_owned(),
        (Some(start), None) => format!("{start}.."),
        (None, Some(count)) => format!("1..{}", 1usize.saturating_add(count)),
        (Some(start), Some(count)) => format!("{start}..{}", start.saturating_add(count)),
    }
}

/// In-memory equivalent of [`stream_slice_lines`], retained for tests
/// that exercise the slicing logic on a string rather than a file.
#[cfg(test)]
pub(crate) fn slice_lines(input: &str, start_line: usize, line_count: Option<usize>) -> ReadSlice {
    let all_lines: Vec<&str> = input.lines().collect();
    let total_lines = all_lines.len();
    let start_idx = start_line.saturating_sub(1).min(total_lines);
    let end_idx = match line_count {
        Some(count) => start_idx.saturating_add(count).min(total_lines),
        None => total_lines,
    };
    ReadSlice {
        content: all_lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{} {line}", start_idx + index + 1))
            .collect::<Vec<_>>()
            .join("\n"),
        line_count: end_idx.saturating_sub(start_idx),
        valid_utf8: true,
        total_lines,
    }
}
