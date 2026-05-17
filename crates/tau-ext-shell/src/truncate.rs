//! Output-truncation helpers shared by every tool.

/// Maximum lines before truncation kicks in.
pub(crate) const MAX_OUTPUT_LINES: usize = 2000;
/// Maximum bytes before truncation kicks in.
pub(crate) const MAX_OUTPUT_BYTES: usize = 50 * 1024;

/// Result of a truncation operation.
pub(crate) struct Truncated {
    pub(crate) content: String,
    pub(crate) was_truncated: bool,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
}

pub(crate) fn truncate_head_plain(input: &str) -> Truncated {
    let total_lines = input.lines().count();
    let total_bytes = input.len();

    if total_lines <= MAX_OUTPUT_LINES && total_bytes <= MAX_OUTPUT_BYTES {
        return Truncated {
            content: input.to_owned(),
            was_truncated: false,
            total_lines,
            total_bytes,
        };
    }

    let mut result = String::new();
    let mut bytes = 0;
    let mut kept_lines = 0;

    for (line_idx, line) in input.lines().enumerate() {
        if kept_lines >= MAX_OUTPUT_LINES || bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
            break;
        }
        if line_idx > 0 {
            result.push('\n');
            bytes += 1;
        }
        result.push_str(line);
        bytes += line.len();
        kept_lines = line_idx + 1;
    }

    Truncated {
        content: result,
        was_truncated: true,
        total_lines,
        total_bytes,
    }
}

/// Truncate from the head (keep first lines).  Used by `read`.
pub(crate) fn truncate_head(input: &str) -> Truncated {
    truncate_head_with_notice(input, "Use start_line and line_count to continue reading.")
}

pub(crate) fn truncate_head_with_notice(input: &str, continuation_hint: &str) -> Truncated {
    let mut truncated = truncate_head_plain(input);
    if !truncated.was_truncated {
        return truncated;
    }

    let kept_lines = truncated.content.lines().count();
    truncated.content.push_str(&format!(
        "\n\n[Showing lines 1-{kept_lines} of {} ({} bytes total). \
         {continuation_hint}]",
        truncated.total_lines, truncated.total_bytes
    ));
    truncated
}

/// Truncate from the tail (keep last lines).  Used by `shell`.
pub(crate) fn truncate_tail(input: &str) -> Truncated {
    truncate_tail_from_tail(input, input.lines().count(), input.len())
}

/// Truncate from a bounded tail suffix while preserving original stream totals.
///
/// `tail` must contain enough of the stream suffix to satisfy the normal shell
/// tail truncation limits. This lets streaming readers discard older output
/// without losing the user-visible truncation marker or total line/byte counts.
pub(crate) fn truncate_tail_from_tail(
    tail: &str,
    total_lines: usize,
    total_bytes: usize,
) -> Truncated {
    if total_lines <= MAX_OUTPUT_LINES && total_bytes <= MAX_OUTPUT_BYTES {
        return Truncated {
            content: tail.to_owned(),
            was_truncated: false,
            total_lines,
            total_bytes,
        };
    }

    let tail_lines: Vec<&str> = tail.lines().collect();

    // Walk backwards, accumulating lines until we hit a limit.
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;

    for &line in tail_lines.iter().rev() {
        let next_bytes = bytes + line.len() + 1;
        if kept.len() < MAX_OUTPUT_LINES && next_bytes <= MAX_OUTPUT_BYTES {
            bytes = next_bytes;
            kept.push(line);
        } else {
            break;
        }
    }
    kept.reverse();

    let content = if kept.is_empty() && !tail.is_empty() {
        let last_line = tail_lines.last().copied().unwrap_or(tail);
        let suffix = valid_utf8_suffix(last_line, MAX_OUTPUT_BYTES);
        let suffix_bytes = suffix.len();
        let line_number = total_lines.max(1);
        format!(
            "[Showing last {suffix_bytes} bytes of line {line_number} of {total_lines} \
             ({total_bytes} bytes total). Line was truncated by byte cap.]\n\n{suffix}"
        )
    } else {
        let first_kept = total_lines - kept.len() + 1;
        let last_kept = total_lines;
        let mut result = format!(
            "[Showing lines {first_kept}-{last_kept} of {total_lines} ({total_bytes} bytes total)]\n\n"
        );
        result.push_str(&kept.join("\n"));
        result
    };

    Truncated {
        content,
        was_truncated: true,
        total_lines,
        total_bytes,
    }
}

fn valid_utf8_suffix(line: &str, max_bytes: usize) -> &str {
    if max_bytes < line.len() {
        let mut start = line.len() - max_bytes;
        while start < line.len() && !line.is_char_boundary(start) {
            start += 1;
        }
        return &line[start..];
    }

    line
}

/// Truncate a single line, appending a marker if truncated.
pub(crate) fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_owned();
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated]", &line[..end])
}
