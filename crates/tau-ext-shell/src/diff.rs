//! Text-diff helpers for the `write` and `edit` tools.

/// Number of unchanged lines to keep around each hunk's edits.
const DIFF_CONTEXT_LINES: usize = 3;

/// Compute a [`tau_proto::DiffSummary`] from two file contents using
/// the `similar` crate. Hunks that are exactly one Remove paired with
/// one Add collapse into a single [`tau_proto::DiffLine::Modify`] with
/// intra-line word-level segments; other shapes flatten to plain
/// Add/Remove/Equal rows.
pub(crate) fn compute_diff(old: &str, new: &str) -> tau_proto::DiffSummary {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut summary = tau_proto::DiffSummary::default();

    for group in diff.grouped_ops(DIFF_CONTEXT_LINES) {
        if group.is_empty() {
            continue;
        }

        // Hunk header (1-based line numbers like unified-diff).
        let first = &group[0];
        let last = &group[group.len() - 1];
        let old_start = first.old_range().start as u32 + 1;
        let new_start = first.new_range().start as u32 + 1;
        let old_count = (last.old_range().end - first.old_range().start) as u32;
        let new_count = (last.new_range().end - first.new_range().start) as u32;

        let mut lines: Vec<tau_proto::DiffLine> = Vec::new();
        // Group adjacent {1×Remove, 1×Add} pairs into Modify lines so
        // single-line edits get intra-line word-level highlighting.
        let mut pending_remove: Option<String> = None;
        for op in &group {
            for change in diff.iter_changes(op) {
                let text = strip_eol(change.value()).to_owned();
                match change.tag() {
                    ChangeTag::Equal => {
                        if let Some(removed) = pending_remove.take() {
                            lines.push(tau_proto::DiffLine::Remove { text: removed });
                        }
                        lines.push(tau_proto::DiffLine::Equal { text });
                    }
                    ChangeTag::Delete => {
                        if let Some(removed) = pending_remove.take() {
                            lines.push(tau_proto::DiffLine::Remove { text: removed });
                        }
                        pending_remove = Some(text);
                        summary.removed += 1;
                    }
                    ChangeTag::Insert => {
                        summary.added += 1;
                        if let Some(removed) = pending_remove.take() {
                            // 1-Remove + 1-Add → Modify with intra-line segments.
                            lines.push(make_modify(&removed, &text));
                        } else {
                            lines.push(tau_proto::DiffLine::Add { text });
                        }
                    }
                }
            }
        }
        if let Some(removed) = pending_remove.take() {
            lines.push(tau_proto::DiffLine::Remove { text: removed });
        }

        summary.hunks.push(tau_proto::DiffHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
        });
    }

    summary
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

fn make_modify(old: &str, new: &str) -> tau_proto::DiffLine {
    use similar::{ChangeTag, TextDiff};
    let inline = TextDiff::from_words(old, new);
    let mut old_segs: Vec<tau_proto::DiffSegment> = Vec::new();
    let mut new_segs: Vec<tau_proto::DiffSegment> = Vec::new();
    for change in inline.iter_all_changes() {
        let text = change.value().to_owned();
        match change.tag() {
            ChangeTag::Equal => {
                old_segs.push(tau_proto::DiffSegment::Equal { text: text.clone() });
                new_segs.push(tau_proto::DiffSegment::Equal { text });
            }
            ChangeTag::Delete => {
                old_segs.push(tau_proto::DiffSegment::Remove { text });
            }
            ChangeTag::Insert => {
                new_segs.push(tau_proto::DiffSegment::Add { text });
            }
        }
    }
    tau_proto::DiffLine::Modify {
        old: old_segs,
        new: new_segs,
    }
}

pub(crate) fn unified_diff(summary: &tau_proto::DiffSummary) -> Option<String> {
    if summary.hunks.is_empty() {
        return None;
    }

    let mut out = String::new();
    for (index, hunk) in summary.hunks.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));
        for line in &hunk.lines {
            match line {
                tau_proto::DiffLine::Equal { text } => {
                    out.push(' ');
                    out.push_str(text);
                    out.push('\n');
                }
                tau_proto::DiffLine::Remove { text } => {
                    out.push('-');
                    out.push_str(text);
                    out.push('\n');
                }
                tau_proto::DiffLine::Add { text } => {
                    out.push('+');
                    out.push_str(text);
                    out.push('\n');
                }
                tau_proto::DiffLine::Modify { old, new } => {
                    out.push('-');
                    out.push_str(&segments_text(old));
                    out.push('\n');
                    out.push('+');
                    out.push_str(&segments_text(new));
                    out.push('\n');
                }
            }
        }
    }
    Some(out)
}

fn segments_text(segments: &[tau_proto::DiffSegment]) -> String {
    let mut text = String::new();
    for segment in segments {
        match segment {
            tau_proto::DiffSegment::Equal { text: segment }
            | tau_proto::DiffSegment::Remove { text: segment }
            | tau_proto::DiffSegment::Add { text: segment } => text.push_str(segment),
        }
    }
    text
}
