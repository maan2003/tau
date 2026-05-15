//! `write` tool: overwrite (or create) a file and report write status.

use std::fs;
use std::path::{Path, PathBuf};

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

    let path_metadata = fs::symlink_metadata(&path_buf).ok();
    let symlink = path_metadata
        .as_ref()
        .filter(|metadata| metadata.file_type().is_symlink())
        .map(|_| symlink_info(&path_buf))
        .transpose()
        .map_err(|error| ToolFailure::from(error.to_string()).with_args(display_args.clone()))?;
    let created = path_metadata.is_none();
    let target_created = symlink
        .as_ref()
        .map(|symlink| symlink.target_created)
        .unwrap_or(false);

    // Read as bytes for correct change detection. The UI diff is text-only,
    // so it is omitted when the previous contents are not valid UTF-8.
    let original_bytes = fs::read(&path_buf).unwrap_or_default();
    let new_bytes = content.as_bytes();
    let changed = created || target_created || original_bytes != new_bytes;
    let original_text = String::from_utf8(original_bytes).ok();

    let bytes_written = content.len();
    fs::write(&path_buf, &content)
        .map_err(|error| ToolFailure::from(error.to_string()).with_args(display_args.clone()))?;

    let diff = original_text
        .as_deref()
        .map(|original| compute_diff(original, &content));

    let display = tau_proto::ToolDisplay {
        args: display_args.clone(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".to_owned(),
        payload: diff.clone().map(ToolDisplayPayload::Diff),
        ..Default::default()
    };
    Ok(ToolOutput {
        result: write_result_value(display_args, bytes_written, created, changed, symlink),
        display,
    })
}

struct SymlinkInfo {
    target: String,
    resolved_target_path: PathBuf,
    target_created: bool,
}

fn symlink_info(path: &Path) -> std::io::Result<SymlinkInfo> {
    let target = fs::read_link(path)?;
    let resolved_target_path = if target.is_absolute() {
        target.clone()
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&target)
    };

    let resolved_target_path = if resolved_target_path.is_absolute() {
        resolved_target_path
    } else {
        std::env::current_dir()?.join(resolved_target_path)
    };
    let target_created = !resolved_target_path.exists();

    Ok(SymlinkInfo {
        target: target.display().to_string(),
        resolved_target_path,
        target_created,
    })
}

fn write_result_value(
    path: String,
    bytes_written: usize,
    created: bool,
    changed: bool,
    symlink: Option<SymlinkInfo>,
) -> CborValue {
    let mut entries = vec![
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
            CborValue::Bool(changed),
        ),
    ];

    if let Some(symlink) = symlink {
        entries.push((
            CborValue::Text("symlink".to_owned()),
            CborValue::Map(vec![
                (
                    CborValue::Text("target".to_owned()),
                    CborValue::Text(symlink.target),
                ),
                (
                    CborValue::Text("resolved_target_path".to_owned()),
                    CborValue::Text(symlink.resolved_target_path.display().to_string()),
                ),
                (
                    CborValue::Text("target_created".to_owned()),
                    CborValue::Bool(symlink.target_created),
                ),
            ]),
        ));
    }

    CborValue::Map(entries)
}
