//! `find` tool: glob-based file search rooted at a directory.

use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use tau_proto::CborValue;

use crate::argument::{argument_text, optional_argument_int, optional_argument_text};
use crate::display::{ToolFailure, ToolOutput, text_stats};
use crate::truncate::truncate_head_plain;

pub(crate) const DEFAULT_FIND_LIMIT: usize = 1000;

pub(crate) fn run_find(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    let pattern = argument_text(arguments, "pattern").map_err(ToolFailure::from)?;
    let path = optional_argument_text(arguments, "path").unwrap_or_else(|| ".".to_owned());
    let limit = optional_argument_int(arguments, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(DEFAULT_FIND_LIMIT);
    let search_path = PathBuf::from(&path);
    let display_args = format!("{pattern} in {}", search_path.display());
    let with_args = |f: ToolFailure| f.with_args(display_args.clone());

    let metadata = fs::metadata(&search_path).map_err(|e| {
        with_args(ToolFailure::from(format!(
            "failed to access {}: {e}",
            search_path.display()
        )))
    })?;
    if !metadata.is_dir() {
        return Err(with_args(ToolFailure::from(format!(
            "not a directory: {}",
            search_path.display()
        ))));
    }

    let glob = compile_find_glob(&pattern).map_err(|e| with_args(ToolFailure::from(e)))?;
    let mut matches = Vec::new();
    for entry in WalkBuilder::new(&search_path)
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
    {
        let entry = entry.map_err(|e| {
            with_args(ToolFailure::from(format!(
                "failed to walk {}: {e}",
                search_path.display()
            )))
        })?;
        let file_type = match entry.file_type() {
            Some(file_type) => file_type,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        let Ok(relative_path) = entry.path().strip_prefix(&search_path) else {
            continue;
        };
        if glob.is_match(relative_path) {
            matches.push(path_to_slash(relative_path));
        }
    }
    matches.sort_by_key(|entry| entry.to_lowercase());

    if matches.is_empty() {
        let mut display = crate::display::ok_display(display_args.clone());
        display.status_text = "ok: no matches".to_owned();
        return Ok(ToolOutput {
            result: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(search_path.display().to_string()),
                ),
                (
                    CborValue::Text("pattern".to_owned()),
                    CborValue::Text(pattern),
                ),
                (
                    CborValue::Text("matches".to_owned()),
                    CborValue::Integer(0.into()),
                ),
                (
                    CborValue::Text("output".to_owned()),
                    CborValue::Text("no files found matching pattern".to_owned()),
                ),
            ]),
            display,
        });
    }

    let total_matches = matches.len();
    let displayed: Vec<String> = matches.into_iter().take(limit).collect();
    let limit_reached = total_matches > displayed.len();
    let mut output_text = displayed.join("\n");
    let truncated = truncate_head_plain(&output_text);
    if truncated.was_truncated {
        output_text = truncated.content;
    }

    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern.",
            limit * 2
        ));
    }
    if truncated.was_truncated {
        notices.push("50KB/2000 line output limit reached.".to_owned());
    }
    if !notices.is_empty() {
        output_text.push_str("\n\n[");
        output_text.push_str(&notices.join(" "));
        output_text.push(']');
    }

    let mut display = crate::display::ok_display(display_args);
    display.stats = text_stats(&output_text);
    Ok(ToolOutput {
        result: CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(search_path.display().to_string()),
            ),
            (
                CborValue::Text("pattern".to_owned()),
                CborValue::Text(pattern),
            ),
            (
                CborValue::Text("matches".to_owned()),
                CborValue::Integer((total_matches as i64).into()),
            ),
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text(output_text),
            ),
        ]),
        display,
    })
}

fn compile_find_glob(pattern: &str) -> Result<GlobSet, String> {
    let glob = Glob::new(pattern).map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder
        .build()
        .map_err(|e| format!("failed to compile glob pattern {pattern:?}: {e}"))
}

fn path_to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
