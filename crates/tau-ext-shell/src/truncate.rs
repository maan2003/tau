//! Output-truncation helpers shared by every tool.

/// Maximum lines before truncation kicks in.
pub(crate) const MAX_OUTPUT_LINES: usize = 2000;
/// Number of leading lines kept when line-count truncation kicks in.
pub(crate) const TRUNCATED_OUTPUT_HEAD_LINES: usize = MAX_OUTPUT_LINES / 2;
/// Number of trailing lines kept when line-count truncation kicks in.
pub(crate) const TRUNCATED_OUTPUT_TAIL_LINES: usize = MAX_OUTPUT_LINES / 2;
/// Maximum bytes before truncation kicks in.
pub(crate) const MAX_OUTPUT_BYTES: usize = 50 * 1024;

/// Result of a truncation operation.
pub(crate) struct Truncated {
    pub(crate) content: String,
    pub(crate) was_truncated: bool,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
}

/// Truncate line-oriented output without adding prose notices.
///
/// When the line count is too high, the first and last 1000 lines are kept with
/// a literal `...` separator between them. Lines that are individually too long
/// are replaced by a marker-only line such as `out(truncated)` so no misleading
/// partial content is shown.
pub(crate) fn truncate_line_oriented(input: &str) -> Truncated {
    let lines: Vec<&str> = input.lines().collect();
    truncate_line_oriented_lines(lines.iter().copied(), lines.len(), input.len())
}

/// Truncate already-rendered line-oriented output with known original totals.
pub(crate) fn truncate_line_oriented_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    total_lines: usize,
    total_bytes: usize,
) -> Truncated {
    let all_lines: Vec<&str> = lines.into_iter().collect();
    let line_count_truncated = MAX_OUTPUT_LINES < total_lines;
    let selected: Vec<Option<&str>> = if line_count_truncated {
        all_lines
            .iter()
            .take(TRUNCATED_OUTPUT_HEAD_LINES)
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
            .chain(
                all_lines
                    .iter()
                    .skip(all_lines.len().saturating_sub(TRUNCATED_OUTPUT_TAIL_LINES))
                    .copied()
                    .map(Some),
            )
            .collect()
    } else {
        all_lines.iter().copied().map(Some).collect()
    };

    let mut rendered = Vec::with_capacity(selected.len());
    let mut rendered_bytes = 0usize;
    let mut was_truncated = line_count_truncated || MAX_OUTPUT_BYTES < total_bytes;
    for line in selected {
        let line = match line {
            Some(line) => line,
            None => {
                rendered.push("...".to_owned());
                rendered_bytes += 3 + usize::from(rendered.len() != 1);
                continue;
            }
        };
        let separator_bytes = usize::from(!rendered.is_empty());
        if MAX_OUTPUT_BYTES < line.len()
            || MAX_OUTPUT_BYTES < rendered_bytes.saturating_add(separator_bytes + line.len())
        {
            rendered.push(mark_line(line, "truncated"));
            rendered_bytes += separator_bytes + rendered.last().map_or(0, String::len);
            was_truncated = true;
        } else {
            rendered.push(line.to_owned());
            rendered_bytes += separator_bytes + line.len();
        }
    }

    Truncated {
        content: rendered.join("\n"),
        was_truncated,
        total_lines,
        total_bytes,
    }
}

/// Add a marker to a rendered line prefix and skip its content.
pub(crate) fn mark_line(line: &str, marker: &str) -> String {
    let prefix = line.split_once(' ').map_or_else(
        || {
            if line.chars().all(|ch| ch.is_ascii_digit()) {
                line
            } else {
                ""
            }
        },
        |(prefix, _)| prefix,
    );
    if let Some((base, existing)) = prefix.split_once('(')
        && let Some(existing) = existing.strip_suffix(')')
    {
        return format!("{base}({existing},{marker})");
    }
    format!("{prefix}({marker})")
}

/// Truncate from the head (keep first and last lines with a separator).
pub(crate) fn truncate_head(input: &str) -> Truncated {
    truncate_line_oriented(input)
}

/// Truncate from the tail (kept for callers that only need line-oriented
/// truncation).
pub(crate) fn truncate_tail(input: &str) -> Truncated {
    truncate_line_oriented(input)
}

/// Truncate a single line by marker, skipping line content.
pub(crate) fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_owned();
    }
    mark_line(line, "truncated")
}
