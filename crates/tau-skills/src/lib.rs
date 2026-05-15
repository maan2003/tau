//! Skill discovery and frontmatter parsing.
//!
//! The frontmatter parser delegates to `serde_yaml_ng`, so the YAML inside
//! `---` fences is the real thing: quoted strings, escapes, block scalars,
//! flow style, comments, anchors. Two project-level conventions on top of
//! that:
//!
//! - Only top-level scalar values (string, bool, number) are exposed. Lists,
//!   mappings and `null` are dropped silently.
//! - All scalars are stringified before being returned. `BTreeMap<String,
//!   String>` is the contract callers see.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml_ng::Value as YamlValue;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A validated, loaded skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    /// When true, the skill is listed in the system prompt at session
    /// start so the agent sees its name + description without having
    /// to search. Opt-in via `advertise: true` in frontmatter; the
    /// default is false so a large skill library doesn't bloat every
    /// prompt — agents discover the rest through `skill { action:
    /// "search", query: "…" }`.
    pub add_to_prompt: bool,
}

impl Skill {
    /// Directory containing this skill's file. Always
    /// `file_path.parent()` (falling back to `file_path` if there is
    /// no parent, which is unreachable for any real on-disk skill).
    pub fn base_dir(&self) -> &Path {
        self.file_path.parent().unwrap_or(&self.file_path)
    }
}

/// Non-fatal diagnostic emitted during skill loading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    /// Soft issue — the skill still loads.
    Warning,
    /// Duplicate name across directories; the first one in wins.
    Collision,
    /// Fatal issue — the skill is not loaded.
    Skipped,
}

/// Result of loading skills from one or more directories.
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const SKILL_FILENAME: &str = "SKILL.md";

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter delimited by `---` lines.
///
/// Returns a map of key→value pairs and the body (content after the closing
/// `---`). If no frontmatter is present, or if the YAML inside the fence
/// fails to parse, returns an empty map and the full content as body.
///
/// Top-level scalars are stringified; non-scalar values (lists, mappings)
/// and `null` are dropped silently — see the module-level docs.
pub fn parse_frontmatter(content: &str) -> (BTreeMap<String, String>, &str) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let Some(rest) = content.strip_prefix("---") else {
        return (BTreeMap::new(), content);
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return (BTreeMap::new(), content);
    };

    let Some((yaml_end, body_start)) = find_closing_fence(rest) else {
        return (BTreeMap::new(), content);
    };

    let yaml_block = &rest[..yaml_end];
    let body = &rest[body_start..];

    let map = match serde_yaml_ng::from_str::<YamlValue>(yaml_block) {
        Ok(YamlValue::Mapping(m)) => m
            .into_iter()
            .filter_map(|(k, v)| {
                let YamlValue::String(key) = k else {
                    return None;
                };
                Some((key, scalar_to_string(&v)?))
            })
            .collect(),
        _ => BTreeMap::new(),
    };

    (map, body)
}

/// Strip frontmatter and return only the body.
pub fn strip_frontmatter(content: &str) -> &str {
    parse_frontmatter(content).1
}

/// Locate the closing `---` fence. Returns `(yaml_end, body_start)` as
/// byte offsets into `s`, where `yaml_end` is the start of the closing
/// fence line and `body_start` is the first byte after that line's
/// terminator (handles both `\n` and `\r\n`).
fn find_closing_fence(s: &str) -> Option<(usize, usize)> {
    let mut pos = 0;
    for line in s.split_inclusive('\n') {
        let stripped = line.trim_end_matches('\n').trim_end_matches('\r');
        if stripped.trim() == "---" {
            return Some((pos, pos + line.len()));
        }
        pos += line.len();
    }
    None
}

/// Stringify a YAML scalar. Non-scalar values (lists, maps, null,
/// tagged) return None and are dropped from the public map.
fn scalar_to_string(v: &YamlValue) -> Option<String> {
    match v {
        YamlValue::String(s) => Some(s.clone()),
        YamlValue::Bool(b) => Some(b.to_string()),
        YamlValue::Number(n) => Some(n.to_string()),
        YamlValue::Null | YamlValue::Sequence(_) | YamlValue::Mapping(_) | YamlValue::Tagged(_) => {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Outcome of name validation. `skip` is true when the name is unusable
/// (empty, wrong charset, badly placed hyphens, too long) — the caller
/// must not produce a `Skill` in that case.
struct NameValidation {
    diagnostics: Vec<SkillDiagnostic>,
    skip: bool,
}

fn validate_name(name: &str, parent_dir_name: Option<&str>, path: &Path) -> NameValidation {
    let mut diagnostics = Vec::new();
    let mut skip = false;

    if name.is_empty() {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name is empty (no `name:` field and no usable parent directory name)"
                .to_owned(),
        });
        return NameValidation {
            diagnostics,
            skip: true,
        };
    }

    if let Some(parent) = parent_dir_name {
        if name != parent {
            diagnostics.push(SkillDiagnostic {
                path: path.to_owned(),
                kind: DiagnosticKind::Warning,
                message: format!("name \"{name}\" does not match parent directory \"{parent}\""),
            });
        }
    }

    if name.len() > MAX_NAME_LENGTH {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!("name exceeds {MAX_NAME_LENGTH} characters ({})", name.len()),
        });
        skip = true;
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        });
        skip = true;
    }

    if name.starts_with('-') || name.ends_with('-') {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not start or end with a hyphen".to_owned(),
        });
        skip = true;
    }

    if name.contains("--") {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not contain consecutive hyphens".to_owned(),
        });
        skip = true;
    }

    NameValidation { diagnostics, skip }
}

fn validate_description(description: &str, path: &Path) -> Vec<SkillDiagnostic> {
    let mut diagnostics = Vec::new();
    if description.len() > MAX_DESCRIPTION_LENGTH {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                description.len()
            ),
        });
    }
    diagnostics
}

// ---------------------------------------------------------------------------
// Single-file loading
// ---------------------------------------------------------------------------

/// Load a single skill from file content and its path on disk.
///
/// Returns `None` for the skill if the description is missing/empty or the
/// name is invalid. Diagnostics are returned in all cases.
pub fn load_skill_from_content(
    content: &str,
    file_path: &Path,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    // The body is intentionally discarded here. Consumers re-read the
    // file on demand via `Skill::file_path` so edits to a skill's
    // instructions are picked up without a daemon restart; caching the
    // body on `Skill` would freeze the contents at discovery time.
    let (fm, _body) = parse_frontmatter(content);

    let skill_dir = file_path.parent().unwrap_or(file_path);
    let parent_dir_name = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned);

    let name = fm
        .get("name")
        .cloned()
        .or_else(|| parent_dir_name.clone())
        .unwrap_or_default();

    let name_check = validate_name(&name, parent_dir_name.as_deref(), file_path);
    diagnostics.extend(name_check.diagnostics);
    if name_check.skip {
        return (None, diagnostics);
    }

    let description = fm.get("description").map(|s| s.trim().to_owned());
    let description = match description {
        Some(d) if !d.is_empty() => {
            diagnostics.extend(validate_description(&d, file_path));
            d
        }
        _ => {
            diagnostics.push(SkillDiagnostic {
                path: file_path.to_owned(),
                kind: DiagnosticKind::Skipped,
                message: "description is required".to_owned(),
            });
            return (None, diagnostics);
        }
    };

    // `advertise: true` opts a skill into the system-prompt listing at
    // session start. Accept case-insensitive `true` or `1`; everything
    // else (including unset) is false.
    let advertise = fm
        .get("advertise")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);

    let skill = Skill {
        name,
        description,
        file_path: file_path.to_owned(),
        add_to_prompt: advertise,
    };

    (Some(skill), diagnostics)
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Discover skill file paths under `root` using Pi-style discovery rules:
///
/// 1. If a directory contains `SKILL.md`, that file is the skill — stop
///    recursing into that directory.
/// 2. Otherwise, at root level only, treat direct `.md` children as individual
///    skills.
/// 3. Recurse into subdirectories to find `SKILL.md`.
/// 4. Skip dot-prefixed entries and `node_modules`.
/// 5. Symlinked directories are checked only for their own `SKILL.md`, without
///    recursing into children.
pub fn discover_skill_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    discover_skill_paths_inner(root, true, &mut paths);
    paths
}

fn discover_skill_paths_inner(dir: &Path, is_root: bool, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let children: Vec<fs::DirEntry> = entries.flatten().collect();

    // Single-pass search: if SKILL.md exists among the children as a regular
    // file, that's this directory's skill and we stop recursing here.
    let skill_md = children.iter().find(|e| {
        if e.file_name() != SKILL_FILENAME {
            return false;
        }
        e.file_type().map(|ft| ft.is_file()).unwrap_or(false)
    });
    if let Some(entry) = skill_md {
        out.push(entry.path());
        return;
    }

    for entry in &children {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };

        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }

        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        let path = entry.path();
        if file_type.is_dir() {
            discover_skill_paths_inner(&path, false, out);
        } else if file_type.is_symlink() {
            scan_symlink_dir_once(&path, out);
        } else if file_type.is_file() && is_root && name_str.ends_with(".md") {
            out.push(path);
        }
    }
}

fn scan_symlink_dir_once(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let children: Vec<fs::DirEntry> = entries.flatten().collect();

    let skill_md = children.iter().find(|e| {
        if e.file_name() != SKILL_FILENAME {
            return false;
        }
        e.file_type().map(|ft| ft.is_file()).unwrap_or(false)
    });
    if let Some(entry) = skill_md {
        out.push(entry.path());
    }
}

// ---------------------------------------------------------------------------
// Multi-directory loading
// ---------------------------------------------------------------------------

/// Load skills from multiple directories, deduplicating by name.
///
/// The first skill with a given name wins; collisions produce a diagnostic.
/// Output skills are sorted by name so successive runs see the same order.
pub fn load_skills_from_dirs(dirs: &[PathBuf]) -> LoadSkillsResult {
    let mut skills_by_name: BTreeMap<String, Skill> = BTreeMap::new();
    let mut all_diagnostics = Vec::new();

    for dir in dirs {
        let paths = discover_skill_paths(dir);
        for path in paths {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    all_diagnostics.push(SkillDiagnostic {
                        path: path.clone(),
                        kind: DiagnosticKind::Warning,
                        message: format!("failed to read: {e}"),
                    });
                    continue;
                }
            };

            let (skill, diags) = load_skill_from_content(&content, &path);
            all_diagnostics.extend(diags);

            if let Some(skill) = skill {
                if let Some(existing) = skills_by_name.get(&skill.name) {
                    all_diagnostics.push(SkillDiagnostic {
                        path: skill.file_path.clone(),
                        kind: DiagnosticKind::Collision,
                        message: format!(
                            "name \"{}\" collision — keeping {}",
                            skill.name,
                            existing.file_path.display()
                        ),
                    });
                } else {
                    skills_by_name.insert(skill.name.clone(), skill);
                }
            }
        }
    }

    LoadSkillsResult {
        skills: skills_by_name.into_values().collect(),
        diagnostics: all_diagnostics,
    }
}

/// Load skills from a single directory.
pub fn load_skills_from_dir(dir: &Path) -> LoadSkillsResult {
    load_skills_from_dirs(&[dir.to_owned()])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
