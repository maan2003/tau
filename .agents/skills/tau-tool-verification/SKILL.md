---
name: tau-tool-verification
description: >
  Use this skill when asked to verify Tau harness tools or tool output behavior,
  especially read, write, edit, shell, line-oriented output, truncation,
  metadata headers, UTF-8 handling, diffs, timeouts, or skill/tool conformance.
---

# Tau Tool Verification

Use when asked to verify Tau skills.

If not explicitly stated, assume the user means `read`, `write`, `edit` and `shell` tools.

## Goal

Your goal is to verify if basic Tau harness tools still work as expected,
and conform to our standards and guidelines.

## Guidelines

### Tool result output structure
All tools should return a HTTP-protocol-like structure:

```
header-1: value-1
header-2: value-2
...
header-n: value-n

multi-line-payload
```

With a single empty line separating headers from the main payload.

`multi-line-payload` can be arbitrary, but line-oriented output typically uses
`<prefix>(optional-per-line-flags) <line-content>` structure. If that's the case
the tool description should mention it.

Many headers are optional, and skipped for their default most natural values
for token efficiency.

### Common patterns

Range operations should use `<start-line>` and `<line-number>` (optional)
approach to range selection.

Newlines are assumed to be `\n`, but other styles are supported
and displayed as `crlf` (`\r\n`), `cr` (`\r`) or `no_nl` (missing trailing newline).

Lines containing invalid UTF-8 characters are skipped, and a `invalid-utf8` is displayed,
and line content is skipped to avoid mistakes and force fallback to more appropriate tools.
In similar way, lines which are too long show `truncated` flag and have content skipped.

Total outputs that are too long are truncated; `truncated: true`,
`total_lines: {lines}` and `total_bytes: {bytes}` headers are added.
These total headers are omitted when output is not truncated.

When output is truncated due to line number limit, first and last 1000 lines
should be shown with `...` line separating them, instead of usual line prefix.
If a single line would exceed the byte budget (currently 50 KB for
`read`/`shell`), show only the line prefix plus `(truncated)` rather than
partial content.


### Tool descriptions

Tool description should be short but informative. They should mention the line prefix meaning, if used in the tool. They should mention line and byte limits.


### Tool-specific guidelines

The output of `read` and `shell` is intentionally similar, and should support
the same semantics. The meaning of the line prefix is different: line number vs stdout/stderr information

`shell` tool will add `duration_seconds: {number}` header for commands that took longer
than 5s to execute. Whole-second precision is acceptable; finer precision is
not needed. Reported durations are approximate, and can include overheads and
latencies of internal components.

`shell` tool should return non-zero exits and timeouts as structured command
results with output details, not as tool invocation errors. It should reliably
timeout operations that take longer than timeout argument, but currently 100%
reliable child process termination is not implemented and will require advanced
techniques to implement in the future (e.g. cgroups).

`edit` tool produces unified-diff like output for edits made in the payload, and
allows at most 100 replacements per call, to limit amount of output it produces.
Requests for more than 100 replacements must error out immediately before making
any changes. If an edit finds no matches, the tool error should include structured
details with `changed: false` and `replacements: 0`. Hunks that would be too large
than some sanity threshold (both lines and bytes) or with invalid characters, will
be replaced with:

```
@@ -1,8 +1,8 @@
<marker>
```

Where marker is similar to ones used in tools like `read`, `shell` output.

Other commands should adhere to pre-existing conventions and naming used in
standard tools.


### Background tools and `wait`

Some tools can run in the background. The agent first receives a synthetic tool result with `kind: background_placeholder` saying:

```
tau_internal: true

Tool call `<tool_call_id>` is running in the background.
```

When the real tool finishes, Tau injects an internal, UI-hidden prompt saying:

```
[tau-internal] Tool call `<tool_call_id>` is complete.
```

The agent can then call `wait` with `tool_call_id` to collect the real result. `wait` is intended for backgrounded calls, but can also wait on a foreground call that is still in flight if the agent already has its call id. Do not call `wait` for such foreground calls in normal use; it wastes tokens compared to letting the tool call finish normally. Prefer telling the user that you will wait for background completion instead of calling `wait` immediately; Tau will wake the agent when the tool is done anyway. If `wait` is used for a backgrounded call, Tau suppresses that internal completion prompt while still emitting the real background result/error event.

Current background timing: most tools background after about 5 seconds, `delegate` backgrounds instantly, and `wait` itself never backgrounds. This may change; when verifying, report if observed behavior differs.

A completed background result is consumed by the first successful `wait`. Later waits for the same id should fail with an already-consumed error. Parallel duplicate waits on the same id race; at most one should receive the result, and the rest should receive either an in-progress duplicate-wait error or an already-consumed error.

When verifying this behavior, check that the synthetic foreground result is visible to the model, the completion notification is delivered to the model but hidden from UI unless `wait` suppressed it, and `wait` returns a completed result once and only once.

### Verification procedure

Create a scratch directory in `/tmp` for your experiments and always avoid dangerous or disruptive actions during testing.

For every tool thoroughly consider all corner cases, including ones which are not covered
in this document.

Report back:

* discrepancies between this document and actual usage,
* things that are wrong, confusing, inconsistent or unclear in both this document and actual tool output
* ideas for improvements both in the tool behavior and this document
