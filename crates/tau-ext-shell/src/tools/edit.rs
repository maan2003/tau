//! `edit` tool: targeted exact-string replacements on a file.

use std::fs;
use std::path::PathBuf;

use tau_proto::{CborValue, ToolDisplay, ToolDisplayPayload, ToolDisplayStatus};

use crate::argument::{argument_array, argument_text, cbor_map_int, cbor_map_text};
use crate::diff::{compute_diff, unified_diff};
use crate::display::{ToolFailure, ToolOutput};

pub(crate) fn edit_file(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let path_buf = PathBuf::from(&path);
    let display_args = path_buf.display().to_string();
    let with_args = |f: ToolFailure| f.with_args(display_args.clone());

    let original =
        fs::read_to_string(&path_buf).map_err(|e| with_args(ToolFailure::from(e.to_string())))?;

    let edits = argument_array(arguments, "edits").map_err(|e| with_args(ToolFailure::from(e)))?;
    if edits.is_empty() {
        return Err(with_args(ToolFailure::new("edits array must not be empty")));
    }

    let line_starts = line_starts(&original);

    // Collect all replacements and validate against the original.
    let mut replacements: Vec<(usize, usize, &str)> = Vec::new();
    for edit in edits {
        let old_text = cbor_map_text(edit, "oldText")
            .ok_or_else(|| with_args(ToolFailure::new("each edit must have a string oldText")))?;
        let new_text = cbor_map_text(edit, "newText")
            .ok_or_else(|| with_args(ToolFailure::new("each edit must have a string newText")))?;
        let max_matches = parse_optional_count(edit, "max_matches", 1, &with_args)?;
        let start_line = parse_optional_line(edit, "start_line", 1, &with_args)?;
        let end_line = parse_optional_line(edit, "end_line", line_starts.len() + 1, &with_args)?;

        if end_line < start_line {
            return Err(with_args(ToolFailure::new(
                "end_line must be greater than or equal to start_line",
            )));
        }

        if old_text.is_empty() {
            return Err(with_args(ToolFailure::new("oldText must not be empty")));
        }

        let start_byte = byte_offset_for_line(&line_starts, start_line, original.len());
        let end_byte = byte_offset_for_line(&line_starts, end_line, original.len());
        for (start, matched) in original[start_byte..end_byte]
            .match_indices(old_text)
            .take(max_matches)
        {
            let start = start_byte + start;
            let end = start + matched.len();
            replacements.push((start, end, new_text));
        }
    }

    // Sort by start position (descending) so we can apply from end to start
    // without invalidating earlier offsets.
    replacements.sort_by_key(|entry| std::cmp::Reverse(entry.0));

    // Check for overlapping ranges.
    for pair in replacements.windows(2) {
        // After descending sort: pair[0].start is later in the file.
        // Overlap if pair[1].end is after pair[0].start.
        if pair[0].0 < pair[1].1 {
            return Err(with_args(ToolFailure::new("overlapping edits")));
        }
    }

    // Apply replacements from end to start.
    let mut result = original.clone();
    for (start, end, new_text) in &replacements {
        result.replace_range(*start..*end, new_text);
    }

    if result != original {
        fs::write(&path_buf, &result).map_err(|e| with_args(ToolFailure::from(e.to_string())))?;
    }

    let diff = compute_diff(&original, &result);

    let display = ToolDisplay {
        args: display_args.clone(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".to_owned(),
        payload: Some(ToolDisplayPayload::Diff(diff.clone())),
        ..Default::default()
    };
    Ok(ToolOutput {
        result: edit_result_value(display_args, replacements.len(), &diff),
        display,
    })
}

fn parse_optional_count(
    edit: &CborValue,
    key: &str,
    default: usize,
    with_args: &dyn Fn(ToolFailure) -> ToolFailure,
) -> Result<usize, ToolFailure> {
    match cbor_map_int(edit, key) {
        Some(n) if n < 0 => Err(with_args(ToolFailure::new(format!(
            "{key} must not be negative"
        )))),
        Some(n) => usize::try_from(n)
            .map_err(|_| with_args(ToolFailure::new(format!("{key} is too large")))),
        None => Ok(default),
    }
}

fn parse_optional_line(
    edit: &CborValue,
    key: &str,
    default: usize,
    with_args: &dyn Fn(ToolFailure) -> ToolFailure,
) -> Result<usize, ToolFailure> {
    match cbor_map_int(edit, key) {
        Some(n) if n < 1 => Err(with_args(ToolFailure::new(format!(
            "{key} must be at least 1"
        )))),
        Some(n) => usize::try_from(n)
            .map_err(|_| with_args(ToolFailure::new(format!("{key} is too large")))),
        None => Ok(default),
    }
}

fn line_starts(input: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in input.bytes().enumerate() {
        if byte == b'\n' && index + 1 < input.len() {
            starts.push(index + 1);
        }
    }
    starts
}

fn byte_offset_for_line(line_starts: &[usize], line: usize, eof: usize) -> usize {
    line_starts
        .get(line.saturating_sub(1))
        .copied()
        .unwrap_or(eof)
}

fn edit_result_value(
    path: String,
    replacements: usize,
    diff: &tau_proto::DiffSummary,
) -> CborValue {
    let mut entries = vec![
        (CborValue::Text("path".to_owned()), CborValue::Text(path)),
        (
            CborValue::Text("replacements".to_owned()),
            CborValue::Integer((replacements as i64).into()),
        ),
        (
            CborValue::Text("changed".to_owned()),
            CborValue::Bool(!diff.hunks.is_empty()),
        ),
    ];
    if let Some(unified) = unified_diff(diff) {
        entries.push((CborValue::Text("diff".to_owned()), CborValue::Text(unified)));
    }
    CborValue::Map(entries)
}
