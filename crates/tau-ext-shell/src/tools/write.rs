//! `write` tool: overwrite (or create) a file and report write status.

use std::fs;
use std::path::PathBuf;

use tau_proto::{CborValue, ToolDisplayPayload, ToolDisplayStatus};

use crate::argument::argument_text;
use crate::diff::compute_diff;
use crate::display::{ToolFailure, ToolOutput};

pub(crate) fn write_file(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let content = argument_text(arguments, "content").map_err(ToolFailure::from)?;
    let path_buf = PathBuf::from(&path);
    let display_args = path_buf.display().to_string();

    if let Some(parent) = path_buf.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|error| {
                ToolFailure::from(error.to_string()).with_args(display_args.clone())
            })?;
        }
    }

    let created = !path_buf.exists();

    // Best-effort read of the existing file so the UI can show a diff.
    // If the file doesn't exist (or can't be decoded as utf-8), treat
    // the baseline as empty — every line of `content` becomes an add.
    let original = fs::read_to_string(&path_buf).unwrap_or_default();

    let bytes_written = content.len();
    fs::write(&path_buf, &content)
        .map_err(|error| ToolFailure::from(error.to_string()).with_args(display_args.clone()))?;

    let diff = compute_diff(&original, &content);

    let display = tau_proto::ToolDisplay {
        args: display_args.clone(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".to_owned(),
        payload: Some(ToolDisplayPayload::Diff(diff.clone())),
        ..Default::default()
    };
    Ok(ToolOutput {
        result: write_result_value(display_args, bytes_written, created, &diff),
        display,
    })
}

fn write_result_value(
    path: String,
    bytes_written: usize,
    created: bool,
    diff: &tau_proto::DiffSummary,
) -> CborValue {
    CborValue::Map(vec![
        (CborValue::Text("path".to_owned()), CborValue::Text(path)),
        (
            CborValue::Text("bytes_written".to_owned()),
            CborValue::Integer((bytes_written as i64).into()),
        ),
        (
            CborValue::Text("created".to_owned()),
            CborValue::Bool(created),
        ),
        (
            CborValue::Text("changed".to_owned()),
            CborValue::Bool(!diff.hunks.is_empty()),
        ),
    ])
}
