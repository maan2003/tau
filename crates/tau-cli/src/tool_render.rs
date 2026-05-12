//! Theming and block rendering for tool calls and other transcript
//! elements. Pure functions over [`tau_proto`] payloads — no
//! [`tau_cli_term`] state lives here.

use std::path::Path;
use std::time::Duration;

use tau_proto::{
    CborValue, ToolDisplay, ToolDisplayPayload, ToolDisplayStatus, cbor_array_field,
    cbor_bool_field, cbor_field, cbor_int_field, cbor_text_field,
};

/// Format the context-usage chip for the status bar. Three cases:
/// - context window known → `" ctx:{percent}%/{window}"` (e.g. `"
///   ctx:6%/200k"`)
/// - window unknown but token count reported → `" ctx:{tokens}/?"`
/// - nothing known yet → empty string (chip suppressed)
pub(crate) fn format_context_chip(
    input_tokens: Option<u64>,
    percent: Option<u8>,
    window: Option<u64>,
) -> String {
    match (window, percent, input_tokens) {
        (Some(w), Some(p), _) => format!(" ctx:{p}%/{}", format_token_count(w)),
        // Window not configured — fall back to raw token count so the
        // user can see usage exists and add `contextWindow` to fix it.
        (None, _, Some(t)) => format!(" ctx:{}/?", format_token_count(t)),
        _ => String::new(),
    }
}

pub(crate) fn format_cache_hit_chip(
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
) -> String {
    match (cache_hit_percent(input_tokens, cached_tokens), input_tokens) {
        (Some(percent), Some(tokens)) => {
            format!(" hit:{percent}%/{}", format_token_count(tokens))
        }
        _ => String::new(),
    }
}

pub(crate) fn format_turn_metrics_chip(latency: Option<Duration>) -> String {
    let mut chip = String::new();
    if let Some(latency) = latency {
        chip.push_str(&format!(" resp:{}", format_latency(latency)));
    }
    chip
}

pub(crate) fn format_token_stats_line(usage: &tau_proto::AgentTokenUsage) -> String {
    let prompt_uncached_tokens = usage
        .prompt_sent_tokens
        .saturating_sub(usage.prompt_cached_tokens);
    let total_uncached_tokens = usage
        .stats
        .total
        .sent_tokens
        .saturating_sub(usage.stats.total.cached_tokens);
    let mut line = format!(
        "↑Δ{}/{} ↑Σ{}/{} ↓Δ{} ↓Σ{}",
        format_token_count(prompt_uncached_tokens),
        format_token_count(usage.prompt_sent_tokens),
        format_token_count(total_uncached_tokens),
        format_token_count(usage.stats.total.sent_tokens),
        format_token_count(usage.response_received_tokens),
        format_token_count(usage.stats.total.received_tokens),
    );
    // Per-turn cache-hit %, only meaningful when this turn actually sent
    // a prompt. Lets the user spot a regression (e.g. tool reordering
    // breaking the prefix) directly on the offending turn instead of
    // inferring it from the cumulative status-bar chip.
    if let Some(percent) = cache_hit_percent(
        Some(usage.prompt_sent_tokens),
        Some(usage.prompt_cached_tokens),
    ) && usage.prompt_sent_tokens > 0
    {
        line.push_str(&format!(" hit:{percent}%"));
    }
    line
}

fn format_latency(latency: Duration) -> String {
    if latency < Duration::from_secs(1) {
        return format!("{}ms", latency.as_millis());
    }
    if latency < Duration::from_secs(10) {
        let tenths = latency.as_millis() / 100;
        let whole = tenths / 10;
        let fractional = tenths % 10;
        return format!("{whole}.{fractional}s");
    }
    format!("{}s", latency.as_secs())
}

pub(crate) fn cache_hit_percent(
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
) -> Option<u8> {
    let input_tokens = input_tokens?;
    let cached_tokens = cached_tokens?;
    if input_tokens == 0 {
        return Some(0);
    }
    let clamped_cached_tokens = cached_tokens.min(input_tokens);
    let percent = clamped_cached_tokens.saturating_mul(100) / input_tokens;
    Some(percent.min(100) as u8)
}

/// Build the iTerm2 OSC 1337 `SetUserVar` escape sequence for the
/// given (name, value) pair, with `value` base64-encoded.
///
/// When `in_tmux` is true the sequence is wrapped in
/// `\x1bPtmux;...\x1b\\` and the inner ESC is doubled so tmux passes
/// the OSC through to the outer terminal instead of consuming it.
/// Mirrors the shape used by the `user-notification.sh` reference
/// script. Caller is responsible for detecting tmux (typically by
/// checking `$TMUX`).
pub(crate) fn build_osc1337_set_user_var(name: &str, value: &str, in_tmux: bool) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    let encoded = STANDARD.encode(value.as_bytes());
    if in_tmux {
        format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar={name}={encoded}\x07\x1b\\")
    } else {
        format!("\x1b]1337;SetUserVar={name}={encoded}\x07")
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let whole = tokens / 1_000;
        let tenth = (tokens % 1_000) / 100;
        if tenth == 0 {
            return format!("{whole}k");
        }
        return format!("{whole}.{tenth}k");
    }
    let whole = tokens / 1_000_000;
    let tenth = (tokens % 1_000_000) / 100_000;
    if tenth == 0 {
        return format!("{whole}m");
    }
    format!("{whole}.{tenth}m")
}

/// Format the `+N/-M` chip from a `DiffSummary` sub-tree on a tool
/// result as themed suffix segments. `+N` is painted with the
/// diff-added style and `-M` with the diff-removed style, matching
/// `git diff --shortstat`. The parens and slash stay in the muted info
/// style. Returns `None` if the diff is missing or empty.
fn format_diff_chip_segments(details: &CborValue) -> Option<Vec<ToolSuffixSegment>> {
    let diff = cbor_field(details, "diff")?;
    let added = cbor_int_field(diff, "added").unwrap_or(0);
    let removed = cbor_int_field(diff, "removed").unwrap_or(0);
    if added == 0 && removed == 0 {
        return None;
    }
    Some(vec![
        info_suffix("(".to_owned()),
        ToolSuffixSegment {
            text: format!("+{added}"),
            status: ToolStatus::DiffAdded,
            no_leading_space: true,
        },
        ToolSuffixSegment {
            text: "/".to_owned(),
            status: ToolStatus::Info,
            no_leading_space: true,
        },
        ToolSuffixSegment {
            text: format!("-{removed}"),
            status: ToolStatus::DiffRemoved,
            no_leading_space: true,
        },
        ToolSuffixSegment {
            text: ")".to_owned(),
            status: ToolStatus::Info,
            no_leading_space: true,
        },
    ])
}

/// Decode a `DiffSummary` sub-tree from a tool result, if present and
/// non-empty. Round-trips the CBOR sub-value through ciborium.
pub(crate) fn extract_diff(details: &CborValue) -> Option<tau_proto::DiffSummary> {
    let diff = cbor_field(details, "diff")?;
    let mut buf = Vec::new();
    ciborium::ser::into_writer(diff, &mut buf).ok()?;
    let summary: tau_proto::DiffSummary = ciborium::de::from_reader(buf.as_slice()).ok()?;
    if summary.added == 0 && summary.removed == 0 {
        return None;
    }
    Some(summary)
}

/// Which status-suffix style the completion block should use.
#[derive(Clone, Copy)]
pub(crate) enum ToolStatus {
    Success,
    Warning,
    Error,
    Info,
    Progress,
    DiffAdded,
    DiffRemoved,
}

#[derive(Clone)]
pub(crate) struct ToolSuffixSegment {
    pub(crate) text: String,
    pub(crate) status: ToolStatus,
    /// When true, suppress the implicit space the renderer normally
    /// inserts before this segment. Used to glue parts of a multi-span
    /// chip (e.g. the colored `+N/-M` diff stat) into one continuous
    /// run.
    pub(crate) no_leading_space: bool,
}

/// Decomposed tool-call label, painted as themed spans:
/// `<tool_name> <args> <suffix...>`.
#[derive(Clone)]
pub(crate) struct ToolCallDisplay {
    pub(crate) tool_name: String,
    pub(crate) args: String,
    pub(crate) suffixes: Vec<ToolSuffixSegment>,
}

/// Builds the display record for a tool call that is still running.
pub(crate) fn format_tool_call(tool_name: &str, arguments: &CborValue) -> ToolCallDisplay {
    let args = match tool_name {
        "shell" => cbor_text_field(arguments, "command").unwrap_or_default(),
        "read" | "write" | "edit" => cbor_text_field(arguments, "path").unwrap_or_default(),
        "find" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            format!("{pattern} in {path}")
        }
        "grep" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            let mut args = format!("{pattern:?} in {path}");
            if let Some(glob) = cbor_text_field(arguments, "glob") {
                args.push_str(&format!(" [{glob}]"));
            }
            args
        }
        "ls" => cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned()),
        "delegate" => match cbor_text_field(arguments, "task_name") {
            Some(name) if !name.is_empty() => format!("[{name}]"),
            _ => String::new(),
        },
        "skill" => match cbor_text_field(arguments, "action").as_deref() {
            Some("search") => {
                let query = cbor_text_field(arguments, "query").unwrap_or_default();
                let scope = if cbor_bool_field(arguments, "search_content").unwrap_or(false) {
                    " [content]"
                } else {
                    ""
                };
                format!("search: {query}{scope}")
            }
            // Default to load semantics for `action: "load"` and for
            // legacy / malformed calls without an action.
            _ => cbor_text_field(arguments, "name").unwrap_or_default(),
        },
        _ => String::new(),
    };
    let suffix = running_suffix_after(&args);
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args,
        suffixes: vec![suffix],
    }
}

/// Builds the running display for a `delegate` call once the harness
/// has reported sub-agent state via `DelegateProgress`. Renders to:
/// `delegate [task_name] ctx: 38%/200k tools: 2/3 …`.
pub(crate) fn format_delegate_progress(
    args: String,
    progress: &tau_proto::DelegateProgress,
) -> ToolCallDisplay {
    let mut suffixes: Vec<ToolSuffixSegment> = Vec::new();
    if progress.ctx_percent.is_some() || progress.ctx_window.is_some() {
        suffixes.push(info_suffix(format_ctx_label(
            progress.ctx_percent,
            progress.ctx_window,
        )));
    }
    let tools_completed = progress
        .tools_total
        .saturating_sub(progress.tools_in_flight);
    suffixes.push(info_suffix(format!(
        "tools: {}/{}",
        tools_completed, progress.tools_total,
    )));
    suffixes.push(running_suffix_after("x")); // non-empty so a leading space is preserved
    ToolCallDisplay {
        tool_name: "delegate".to_owned(),
        args,
        suffixes,
    }
}

/// Builds the completion display for a finished `delegate` call.
/// Renders to:
/// `delegate [task_name] ctx: 38%/200k tools: 3 (5L, 220B) ok`
/// (or with `err: …` when `error_message` is set). The chip order
/// mirrors the in-progress line — `ctx:` then `tools:` — so the
/// transition from running to done shifts only the trailing segments
/// (`…` → `(NL, NB) ok`) instead of shuffling existing ones around.
pub(crate) fn format_delegate_completion(
    args: String,
    last_progress: Option<&tau_proto::DelegateProgress>,
    details: &CborValue,
    error_message: Option<&str>,
) -> ToolCallDisplay {
    let response_text = match details {
        CborValue::Text(text) => text.as_str(),
        _ => "",
    };
    let mut suffixes: Vec<ToolSuffixSegment> = Vec::new();
    // Match the in-progress order (`ctx:` → `tools:`) so the line
    // doesn't visibly reorder when it transitions to done.
    if let Some(progress) = last_progress {
        if progress.ctx_percent.is_some() || progress.ctx_window.is_some() {
            suffixes.push(info_suffix(format_ctx_label(
                progress.ctx_percent,
                progress.ctx_window,
            )));
        }
        // Show a single number on completion since "in flight" is
        // always zero by the time the result lands; total is the
        // bit the user cares about.
        suffixes.push(info_suffix(format!("tools: {}", progress.tools_total)));
    }
    if !response_text.is_empty() {
        suffixes.push(output_stats_suffix(response_text));
    }
    suffixes.push(match error_message {
        Some(msg) if !msg.is_empty() => err_suffix(Some(msg)),
        _ => ok_suffix(),
    });
    ToolCallDisplay {
        tool_name: "delegate".to_owned(),
        args,
        suffixes,
    }
}

/// Renders the `ctx: ` chip for a `DelegateProgress` snapshot. Falls
/// back to whichever side of (`%`, `window`) is known.
fn format_ctx_label(percent: Option<u8>, window: Option<u64>) -> String {
    let percent_part = percent
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "?".to_owned());
    let window_part = window.map(format_window).unwrap_or_default();
    if window_part.is_empty() {
        format!("ctx: {percent_part}")
    } else {
        format!("ctx: {percent_part}/{window_part}")
    }
}

/// Compact rendering of a context-window size. `200000` -> `200k`,
/// `1_048_576` -> `1.0M`. Approximate; for surfacing alongside a `%`.
fn format_window(window: u64) -> String {
    if window >= 1_000_000 {
        let m = window as f64 / 1_000_000.0;
        format!("{m:.1}M")
    } else if window >= 1_000 {
        format!("{}k", window / 1_000)
    } else {
        window.to_string()
    }
}

fn tool_suffix(text: String, status: ToolStatus) -> ToolSuffixSegment {
    ToolSuffixSegment {
        text,
        status,
        no_leading_space: false,
    }
}

fn info_suffix(text: String) -> ToolSuffixSegment {
    tool_suffix(text, ToolStatus::Info)
}

/// Build the running-call ellipsis with the same leading-space rule
/// `append_streaming_indicator` applies: skip the implicit space the
/// renderer would otherwise insert when the preceding text (`args`)
/// already ends in whitespace. Empty `args` keeps the space, since the
/// label preceding the suffix is then the tool name (never whitespace).
fn running_suffix_after(args: &str) -> ToolSuffixSegment {
    let no_leading_space = args.chars().next_back().is_some_and(char::is_whitespace);
    ToolSuffixSegment {
        text: "…".to_owned(),
        status: ToolStatus::Progress,
        no_leading_space,
    }
}

fn ok_suffix() -> ToolSuffixSegment {
    tool_suffix("ok".to_owned(), ToolStatus::Success)
}

fn err_suffix(message: Option<&str>) -> ToolSuffixSegment {
    match message {
        Some(msg) if !msg.is_empty() => tool_suffix(format!("err: {msg}"), ToolStatus::Error),
        _ => tool_suffix("err".to_owned(), ToolStatus::Error),
    }
}

/// Build a streaming block whose body uses `body_name` styling and
/// whose trailing `…` indicator uses [`names::PROGRESS_INDICATOR`], so
/// the indicator can be themed independently. The leading space before
/// the indicator is skipped when the body is empty or already ends in
/// whitespace, so the `…` doesn't double up whitespace or land one
/// column off the left margin on a fresh line.
pub(crate) fn streaming_block(
    theme: &tau_themes::Theme,
    body_name: &str,
    body_text: impl Into<String>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::{convert_color, resolve};
    use tau_cli_term::{Span, Style, StyledBlock, StyledText};
    use tau_themes::{StyleName, names};

    let body_text = body_text.into();
    let needs_space = body_text
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());

    let body_ts = theme.resolve_style(&StyleName::new(body_name));
    let body_span_style = Style {
        fg: body_ts.fg.map(convert_color),
        bg: None,
        bold: body_ts.bold,
        underline: body_ts.underline,
        italic: body_ts.italic,
    };
    let progress_style = resolve(theme, names::PROGRESS_INDICATOR);

    let mut spans = Vec::with_capacity(3);
    if !body_text.is_empty() {
        spans.push(Span::new(body_text, body_span_style));
    }
    if needs_space {
        spans.push(Span::new(" ", body_span_style));
    }
    spans.push(Span::new("…".to_owned(), progress_style));

    let mut block = StyledBlock::new(StyledText::from(spans));
    if let Some(bg) = body_ts.bg {
        block = block.bg(convert_color(bg));
    }
    block
}

fn output_stats_suffix(text: &str) -> ToolSuffixSegment {
    stats_suffix(None, text)
}

fn stats_suffix(prefix: Option<String>, text: &str) -> ToolSuffixSegment {
    let mut parts = Vec::new();
    if let Some(prefix) = prefix {
        parts.push(prefix);
    }
    parts.push(format!("{}L", text.lines().count()));
    parts.push(format!("{}B", text.len()));
    info_suffix(format!("({})", parts.join(", ")))
}

fn websearch_stats_suffix(text: &str) -> ToolSuffixSegment {
    let titles = text
        .lines()
        .filter(|line| line.starts_with("Title:"))
        .count();
    let urls = text.lines().filter(|line| line.starts_with("URL:")).count();
    let results = if titles < urls { urls } else { titles };
    if 0 < results {
        info_suffix(format!(
            "({results} results, {}L, {}B)",
            text.lines().count(),
            text.len()
        ))
    } else {
        output_stats_suffix(text)
    }
}

/// Render the `queries` array from a skill search/load-error result
/// as a single space-separated string. Non-text entries are skipped
/// rather than failing the render.
fn format_skill_query_list(queries: &[CborValue]) -> String {
    let mut out = String::new();
    for q in queries {
        if let CborValue::Text(s) = q
            && !s.is_empty()
        {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(s);
        }
    }
    out
}

/// Error-path display: `<tool_name> <args> <err>`.
fn format_tool_error(tool_name: &str, args: String, error_message: &str) -> ToolCallDisplay {
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args,
        suffixes: vec![err_suffix(Some(error_message))],
    }
}

/// Render a [`ToolDisplay`] descriptor directly to a
/// [`ToolCallDisplay`]. The generic path the renderer takes when the
/// tool side attached a display descriptor to its result/error event —
/// no `match tool_name` arms needed. Falls back to
/// [`format_tool_completion`] for older events that didn't carry a
/// descriptor.
pub(crate) fn render_tool_display(tool_name: &str, display: &ToolDisplay) -> ToolCallDisplay {
    let mut suffixes: Vec<ToolSuffixSegment> = Vec::new();
    // Diff `+N -M` chips (themed green/red) are derived from the
    // payload so write/edit don't have to push them as info chips.
    if let Some(ToolDisplayPayload::Diff(summary)) = &display.payload
        && (summary.added > 0 || summary.removed > 0)
    {
        if summary.added > 0 {
            suffixes.push(tool_suffix(
                format!("+{}", summary.added),
                ToolStatus::DiffAdded,
            ));
        }
        if summary.removed > 0 {
            suffixes.push(ToolSuffixSegment {
                text: format!("-{}", summary.removed),
                status: ToolStatus::DiffRemoved,
                no_leading_space: summary.added > 0,
            });
        }
    }
    let stats_chip = format_tool_display_stats(&display.stats);
    if !stats_chip.is_empty() {
        suffixes.push(info_suffix(stats_chip));
    }
    for chip in &display.info_chips {
        suffixes.push(info_suffix(chip.clone()));
    }
    let status_kind = match display.status {
        ToolDisplayStatus::Success => ToolStatus::Success,
        ToolDisplayStatus::Warning => ToolStatus::Warning,
        ToolDisplayStatus::Error => ToolStatus::Error,
    };
    suffixes.push(tool_suffix(display.status_text.clone(), status_kind));
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args: display.args.clone(),
        suffixes,
    }
}

fn format_tool_display_stats(stats: &tau_proto::ToolDisplayStats) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = stats.matches {
        parts.push(format!("{m}M"));
    }
    if let Some(l) = stats.lines {
        parts.push(format!("{l}L"));
    }
    if let Some(b) = stats.bytes {
        parts.push(format_tool_display_bytes(b));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    }
}

fn format_tool_display_bytes(bytes: u64) -> String {
    if bytes >= 1024 {
        let k = bytes as f64 / 1024.0;
        if k >= 100.0 {
            format!("{k:.0}kB")
        } else {
            format!("{k:.1}kB")
        }
    } else {
        format!("{bytes}B")
    }
}

/// Formats a completed tool call for display.
pub(crate) fn format_tool_completion(
    tool_name: &str,
    details: &CborValue,
    error_message: Option<&str>,
) -> ToolCallDisplay {
    match tool_name {
        "shell" => format_shell_completion(details, error_message),
        "read" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("read", path, msg)
            } else {
                let content = cbor_text_field(details, "content").unwrap_or_default();
                ToolCallDisplay {
                    tool_name: "read".into(),
                    args: path,
                    suffixes: vec![output_stats_suffix(&content), ok_suffix()],
                }
            }
        }
        "write" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("write", path, msg)
            } else {
                // Prefer the colored +N/-M diff chip; fall back to byte
                // count for tools that don't ship a diff (or no-op
                // writes).
                let mut suffixes = format_diff_chip_segments(details).unwrap_or_else(|| {
                    let bytes = cbor_int_field(details, "bytes_written").unwrap_or(0);
                    vec![info_suffix(format!("({bytes}B)"))]
                });
                suffixes.push(ok_suffix());
                ToolCallDisplay {
                    tool_name: "write".into(),
                    args: path,
                    suffixes,
                }
            }
        }
        "edit" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("edit", path, msg)
            } else {
                let mut suffixes = format_diff_chip_segments(details).unwrap_or_else(|| {
                    let count = cbor_int_field(details, "edits_applied").unwrap_or(0);
                    vec![info_suffix(format!("({count} edits applied)"))]
                });
                suffixes.push(ok_suffix());
                ToolCallDisplay {
                    tool_name: "edit".into(),
                    args: path,
                    suffixes,
                }
            }
        }
        "find" => {
            let path = cbor_text_field(details, "path").unwrap_or_else(|| ".".to_owned());
            let pattern = cbor_text_field(details, "pattern").unwrap_or_default();
            let args = format!("{pattern} in {path}");
            if let Some(msg) = error_message {
                format_tool_error("find", args, msg)
            } else {
                let output = cbor_text_field(details, "output").unwrap_or_default();
                ToolCallDisplay {
                    tool_name: "find".into(),
                    args,
                    suffixes: vec![stats_suffix(None, &output), ok_suffix()],
                }
            }
        }
        "grep" => {
            let path = cbor_text_field(details, "path").unwrap_or_else(|| ".".to_owned());
            let pattern = cbor_text_field(details, "pattern").unwrap_or_default();
            let glob = cbor_text_field(details, "glob");
            let args = match glob {
                Some(g) => format!("{pattern:?} in {path} [{g}]"),
                None => format!("{pattern:?} in {path}"),
            };
            if let Some(msg) = error_message {
                format_tool_error("grep", args, msg)
            } else {
                let output = cbor_text_field(details, "output").unwrap_or_default();
                let status = cbor_int_field(details, "status");
                let match_count = cbor_int_field(details, "matches").unwrap_or(0);
                let mut suffixes = vec![stats_suffix(Some(format!("{match_count}M")), &output)];
                suffixes.push(match status {
                    Some(0) => ok_suffix(),
                    Some(1) => tool_suffix("ok: no matches".to_owned(), ToolStatus::Success),
                    Some(code) => err_suffix(Some(&code.to_string())),
                    None => info_suffix("ok?".to_owned()),
                });
                ToolCallDisplay {
                    tool_name: "grep".into(),
                    args,
                    suffixes,
                }
            }
        }
        "ls" => {
            let path = cbor_text_field(details, "path").unwrap_or_else(|| ".".to_owned());
            if let Some(msg) = error_message {
                format_tool_error("ls", path, msg)
            } else {
                let count = cbor_int_field(details, "entries").unwrap_or(0);
                ToolCallDisplay {
                    tool_name: "ls".into(),
                    args: path,
                    suffixes: vec![info_suffix(format!("({count} entries)")), ok_suffix()],
                }
            }
        }
        "websearch_exa" => {
            if let Some(msg) = error_message {
                format_tool_error("websearch_exa", String::new(), msg)
            } else {
                let text = match details {
                    CborValue::Text(text) => text.as_str(),
                    _ => "",
                };
                ToolCallDisplay {
                    tool_name: "websearch_exa".into(),
                    args: String::new(),
                    suffixes: vec![websearch_stats_suffix(text), ok_suffix()],
                }
            }
        }
        // `delegate` is rendered by `format_delegate_completion`,
        // which has access to the cached task name + last progress
        // snapshot. This match arm is unreachable for the running
        // delegate path; if a synthetic result somehow flows through
        // the generic fallback it will land in the catch-all below.
        "skill" => {
            // Distinguish search vs load by the result shape: search
            // results carry `queries` + `matches`; load successes
            // carry `name` + `content`; load failures carry `name`
            // plus a search-shaped `queries` + `matches` echo built
            // from the requested name's word-like tokens.
            let queries = cbor_array_field(details, "queries")
                .map(format_skill_query_list)
                .filter(|s| !s.is_empty());
            let load_name = cbor_text_field(details, "name");
            let match_count = cbor_array_field(details, "matches")
                .map(<[CborValue]>::len)
                .unwrap_or(0);
            let scope = if cbor_bool_field(details, "search_content").unwrap_or(false) {
                " [content]"
            } else {
                ""
            };
            match (queries, load_name.clone(), error_message) {
                // Failed load with auto-search hint. Show the
                // requested name as args and surface the suggestion
                // count next to the error so the user can tell at a
                // glance whether to expect useful follow-ups.
                (Some(_), Some(name), Some(msg)) => ToolCallDisplay {
                    tool_name: "skill".into(),
                    args: name,
                    suffixes: vec![
                        info_suffix(format!("({match_count} suggestions)")),
                        err_suffix(Some(msg)),
                    ],
                },
                // Plain skill search (success or error).
                (Some(query_str), _, error_message) => {
                    let args = format!("search: {query_str}{scope}");
                    if let Some(msg) = error_message {
                        format_tool_error("skill", args, msg)
                    } else {
                        ToolCallDisplay {
                            tool_name: "skill".into(),
                            args,
                            suffixes: vec![info_suffix(format!("({match_count}L)")), ok_suffix()],
                        }
                    }
                }
                // Plain skill load (success or non-suggestion error).
                (None, _, error_message) => {
                    let name = load_name.unwrap_or_default();
                    if let Some(msg) = error_message {
                        format_tool_error("skill", name, msg)
                    } else {
                        let content = cbor_text_field(details, "content").unwrap_or_default();
                        ToolCallDisplay {
                            tool_name: "skill".into(),
                            args: name,
                            suffixes: vec![output_stats_suffix(&content), ok_suffix()],
                        }
                    }
                }
            }
        }
        _ => ToolCallDisplay {
            tool_name: tool_name.to_owned(),
            args: String::new(),
            suffixes: vec![match error_message {
                Some(msg) => err_suffix(Some(msg)),
                None => ok_suffix(),
            }],
        },
    }
}

fn format_shell_completion(details: &CborValue, error_message: Option<&str>) -> ToolCallDisplay {
    let cmd = cbor_text_field(details, "command").unwrap_or_default();
    if !matches!(details, CborValue::Map(_))
        && let Some(msg) = error_message
    {
        return format_tool_error("shell", cmd, msg);
    }

    let stdout = cbor_text_field(details, "stdout").unwrap_or_default();
    let stderr = cbor_text_field(details, "stderr").unwrap_or_default();
    let combined = if stdout.is_empty() {
        stderr.clone()
    } else if stderr.is_empty() {
        stdout.clone()
    } else {
        format!("{stdout}\n{stderr}")
    };

    let status = cbor_int_field(details, "status");
    let suffixes = vec![
        output_stats_suffix(&combined),
        match status {
            Some(0) => ok_suffix(),
            Some(code) => err_suffix(Some(&code.to_string())),
            None => info_suffix("ok?".to_owned()),
        },
    ];
    ToolCallDisplay {
        tool_name: "shell".into(),
        args: cmd,
        suffixes,
    }
}

/// Paints a [`ToolCallDisplay`] onto a themed block.
pub(crate) fn render_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::StyledBlock;
    use tau_cli_term::resolve::themed_text;
    use tau_themes::{SpanTree, ThemedText, names};

    let mut themed = ThemedText::new();
    let output = themed.add_style(names::TOOL_OUTPUT);
    let name = themed.add_style(names::TOOL_NAME);
    let args = themed.add_style(names::TOOL_ARGS);

    let mut children = vec![SpanTree::span(
        name,
        vec![SpanTree::text(display.tool_name.clone())],
    )];
    if !display.args.is_empty() {
        children.push(SpanTree::span(
            args,
            vec![SpanTree::text(" "), SpanTree::text(display.args.clone())],
        ));
    }
    for suffix in &display.suffixes {
        let status_name = match suffix.status {
            ToolStatus::Success => names::TOOL_STATUS_SUCCESS,
            // Warning has no dedicated token yet — share the info
            // colour so the chip still reads as "non-error" without a
            // theme migration.
            ToolStatus::Warning | ToolStatus::Info => names::TOOL_STATUS_INFO,
            ToolStatus::Error => names::TOOL_STATUS_ERROR,
            ToolStatus::Progress => names::PROGRESS_INDICATOR,
            ToolStatus::DiffAdded => names::DIFF_ADDED,
            ToolStatus::DiffRemoved => names::DIFF_REMOVED,
        };
        let status = themed.add_style(status_name);
        if !suffix.no_leading_space && !suffix.text.starts_with(':') {
            children.push(SpanTree::span(args, vec![SpanTree::text(" ")]));
        }
        children.push(SpanTree::span(
            status,
            vec![SpanTree::text(suffix.text.clone())],
        ));
    }
    themed.push_tree(SpanTree::span(output, children));

    StyledBlock::new(themed_text(theme, &themed))
}

/// Like [`render_tool_block`] but appends an expanded unified-diff
/// body when `expanded` is true and `diff` has hunks. The first line
/// is the themed tool header (with `+N/-M` chip); the body, if
/// rendered, comes after a `\n` so `layout_lines` wraps each diff line
/// independently.
pub(crate) fn render_diff_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
    diff: &tau_proto::DiffSummary,
    expanded: bool,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    // Reuse the header from render_tool_block, then keep its spans so
    // we can append diff lines below it.
    let header = render_tool_block(theme, display);
    let mut spans: Vec<Span> = header.content.spans().to_vec();

    if !expanded || diff.hunks.is_empty() {
        return StyledBlock::new(StyledText::from(spans));
    }

    let added_style = resolve(theme, names::DIFF_ADDED);
    let removed_style = resolve(theme, names::DIFF_REMOVED);
    let context_style = resolve(theme, names::DIFF_CONTEXT);
    let header_style = resolve(theme, names::DIFF_HUNK_HEADER);
    let added_inline_style = resolve(theme, names::DIFF_ADDED_INLINE);
    let removed_inline_style = resolve(theme, names::DIFF_REMOVED_INLINE);

    for hunk in &diff.hunks {
        spans.push(Span::new("\n", context_style));
        spans.push(Span::new(
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
            header_style,
        ));
        for line in &hunk.lines {
            spans.push(Span::new("\n", context_style));
            match line {
                tau_proto::DiffLine::Equal { text } => {
                    spans.push(Span::new(format!("  {text}"), context_style));
                }
                tau_proto::DiffLine::Add { text } => {
                    spans.push(Span::new(format!("+ {text}"), added_style));
                }
                tau_proto::DiffLine::Remove { text } => {
                    spans.push(Span::new(format!("- {text}"), removed_style));
                }
                tau_proto::DiffLine::Modify { old, new } => {
                    spans.push(Span::new("- ".to_owned(), removed_style));
                    push_segments(&mut spans, old, removed_style, removed_inline_style);
                    spans.push(Span::new("\n".to_owned(), context_style));
                    spans.push(Span::new("+ ".to_owned(), added_style));
                    push_segments(&mut spans, new, added_style, added_inline_style);
                }
            }
        }
    }
    StyledBlock::new(StyledText::from(spans))
}

fn push_segments(
    spans: &mut Vec<tau_cli_term::Span>,
    segments: &[tau_proto::DiffSegment],
    base: tau_cli_term::Style,
    inline: tau_cli_term::Style,
) {
    use tau_cli_term::Span;
    for seg in segments {
        match seg {
            tau_proto::DiffSegment::Equal { text } => {
                spans.push(Span::new(text.clone(), base));
            }
            // Within a Modify line, only the *changed* sub-slice on
            // each side is meaningful. Hide the *other* side's slice
            // so we don't double up (e.g. the - line shouldn't show
            // the new tokens, only the old).
            tau_proto::DiffSegment::Remove { text } => {
                spans.push(Span::new(text.clone(), inline));
            }
            tau_proto::DiffSegment::Add { text } => {
                spans.push(Span::new(text.clone(), inline));
            }
        }
    }
}

/// Render a user `!`/`!!` shell block: a `shell <cmd>` header in the
/// same three-span theme used for tool calls, with streaming output
/// below in the default style.
///
/// `status_suffix`:
///   - `Some("running")` while the command is in-flight (info style),
///   - `Some("[0]")` / `Some("[N]")` on completion (success / error style,
///     keyed off exit code),
///   - `Some("cancelled")` on cancel (info style).
pub(crate) fn render_shell_block(
    theme: &tau_themes::Theme,
    command: &str,
    output: &str,
    status_suffix: Option<&str>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    let name_style = resolve(theme, names::TOOL_NAME);
    let args_style = resolve(theme, names::TOOL_ARGS);
    let status_name = match status_suffix {
        Some(s) if s.starts_with("[0]") => names::TOOL_STATUS_SUCCESS,
        Some(s) if s.starts_with('[') => names::TOOL_STATUS_ERROR,
        _ => names::TOOL_STATUS_INFO,
    };
    let status_style = resolve(theme, status_name);

    let mut spans = vec![
        Span::new("shell", name_style),
        Span::new(" ", args_style),
        Span::new(command.to_owned(), args_style),
    ];
    if let Some(suffix) = status_suffix {
        spans.push(Span::new(" ", args_style));
        spans.push(Span::new(suffix.to_owned(), status_style));
    }
    if !output.is_empty() {
        spans.push(Span::new("\n", args_style));
        spans.push(Span::new(output.to_owned(), args_style));
    }
    StyledBlock::new(StyledText::from(spans))
}

pub(crate) fn render_harness_info(
    theme: &tau_themes::Theme,
    info: &tau_proto::HarnessInfo,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::themed_block;
    use tau_themes::names;

    if info.level == tau_proto::HarnessInfoLevel::Normal
        && let Some(path) = info
            .message
            .strip_prefix("session dir: ")
            .and_then(|path| path.strip_suffix('/'))
    {
        return system_path_block(theme, "session dir: ", Path::new(path), "/");
    }

    let style_name = match info.level {
        tau_proto::HarnessInfoLevel::Normal => names::SYSTEM_INFO,
        tau_proto::HarnessInfoLevel::Important => names::SYSTEM_INFO_IMPORTANT,
    };
    themed_block(theme, style_name, &info.message)
}

pub(crate) fn ui_dir_block(theme: &tau_themes::Theme, path: &Path) -> tau_cli_term::StyledBlock {
    system_path_block(theme, "ui dir: ", path, "/")
}

pub(crate) fn session_status_block(
    theme: &tau_themes::Theme,
    path: &Path,
    suffix: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let lifecycle = text.add_style(names::EXTENSION_LIFECYCLE);
    let status_style = text.add_style(names::SESSION_STATUS);
    let path_style = text.add_style(names::SYSTEM_PATH);
    text.push(lifecycle, "session dir: ");
    text.push(path_style, format!("{}{}", display_path(path), suffix));
    text.push(lifecycle, " ");
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn system_path_block(
    theme: &tau_themes::Theme,
    prefix: &str,
    path: &Path,
    suffix: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let path_style = text.add_style(names::SYSTEM_PATH);
    text.push(info, prefix);
    text.push(path_style, format!("{}{}", display_path(path), suffix));
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

pub(crate) fn system_loaded_block(
    theme: &tau_themes::Theme,
    path: &Path,
    content: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let path_style = text.add_style(names::SYSTEM_PATH);
    let stats_style = text.add_style(names::TOOL_STATUS_INFO);
    text.push(info, "loaded: ");
    text.push(path_style, display_path(path));
    text.push(info, " ");
    text.push(stats_style, output_stats_suffix(content).text);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

pub(crate) fn system_status_block(
    theme: &tau_themes::Theme,
    prefix: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let status_style = text.add_style(names::SYSTEM_STATUS);
    text.push(info, prefix);
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

pub(crate) fn extension_status_block(
    theme: &tau_themes::Theme,
    extension_name: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let lifecycle = text.add_style(names::EXTENSION_LIFECYCLE);
    let status_style = text.add_style(names::EXTENSION_STATUS);
    text.push(lifecycle, "extension ");
    text.push(lifecycle, extension_name);
    text.push(lifecycle, " ");
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn display_path(path: &Path) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.display().to_string();
    };
    let home = Path::new(&home);
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    let Ok(suffix) = path.strip_prefix(home) else {
        return path.display().to_string();
    };
    if suffix.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", suffix.display())
    }
}
