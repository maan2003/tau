# Shell tool fixes TODO

Status tracker and agreed plan for fixing `tau-ext-shell` shell-tool interface issues.

## Workflow

- Discuss one issue at a time, from easiest to hardest.
- After agreeing on a solution, delegate implementation to a sub-agent.
- Review results and update this file before moving to the next issue.
- Add regression tests for every behavior fix.

## Status legend

- `open` — identified, solution not agreed.
- `agreed` — plan agreed, not implemented.
- `delegated` — implementation in progress by sub-agent.
- `done` — implemented and verified.
- `wontfix` — explicitly accepted as-is.

## Issues

### 1. Invalid UTF-8 output is silently lost

Status: `done`

Done note: tool-side pipe output now uses lossy UTF-8 decoding, warns in stderr when invalid bytes were replaced, and has regression tests for invalid stdout, invalid stderr, and both streams.

Plan:

- Replace tool-side `read_to_string()` pipe collection with byte collection and lossy UTF-8 decoding.
- Detect whether stdout and/or stderr contained invalid UTF-8.
- Prefix stderr result with a warning:
  - `[tau-shell] warning: stdout contained non-UTF-8 bytes; invalid bytes were replaced`
  - `[tau-shell] warning: stderr contained non-UTF-8 bytes; invalid bytes were replaced`
  - or one combined warning when both streams were affected.
- Keep user `!` shell behavior consistent where practical.
- Add regression tests for invalid stdout, invalid stderr, and both streams.

### 2. Timeout is not a hard wall-clock bound with background processes

Status: `done`

Done note: Unix shell-tool output reading now uses nonblocking poll over stdout, stderr, and a child-exit wake pipe, so foreground exit or timeout returns after a brief drain without waiting for background pipe EOF.

Problem:

- The shell process can exit before timeout, while background descendants keep stdout/stderr pipes open.
- Reader thread joins can block until those descendants exit.

Agreed plan:

- Replace blocking reader threads with readiness-based nonblocking pipe reads.
- Use `poll`/equivalent readiness waiting, not busy polling.
- Completion rule:
  - foreground command exits, then briefly drain currently available pipe output and return;
  - timeout fires, kill the process group, briefly drain currently available output and return;
  - background children that keep pipes open after shell exit do not keep the tool alive forever.
- Add regression tests proving a background pipe holder cannot exceed timeout indefinitely.
- Combine with issue 14 by using bounded per-stream tail buffers while reading.

### 3. Detached grandchildren can survive timeout

Status: `open`

Problem:

- A descendant can call `setsid` and escape the shell process group.
- Process-group kill on timeout does not reach it.

Current decision:

- First make timeout return reliable via issue 2.
- Defer full escaped-grandchild cleanup.

Options still open:

- Linux cgroup per shell invocation, kill the cgroup on timeout. Strongest cleanup on Linux.
- Linux pidfd plus recursive process-tree kill. Race-prone and misses double-fork/session escape cases.
- Subreaper plus process-tree tracking. Helps with orphan reaping, not perfect for all escape patterns.
- Document as impossible to fully guarantee without OS containment, but make timeout return reliable regardless of escaped children.

### 4. Detached/background children can delay timeout return

Status: `done`

Done note: timeout return is now independent from inherited stdout/stderr pipe EOF, including escaped children that keep pipes open; escaped-grandchild cleanup still needs the issue 3 cgroup/containment decision.

Problem:

- Same root as issues 2 and 3: escaped or background descendants can keep stdout/stderr open after timeout.

Agreed plan:

- Solve together with issue 2 by making timeout return independent from pipe EOF.
- If cgroups are chosen later for issue 3, use them to clean up escaped descendants too.

### 5. Single huge line truncation is broken

Status: `done`

Done note: tail truncation now falls back to a valid UTF-8 byte suffix for oversized final lines, marks byte-level line truncation clearly, warns in shell stderr when streams are truncated, and has regression tests for huge-line cases.

Problem:

- `truncate_tail()` can keep zero lines when one line exceeds the byte cap, yielding ranges like `lines 2-1 of 1`.

Agreed plan:

- Tail-truncate by bytes as a fallback when no whole line fits, preserving a valid UTF-8 suffix of the oversized line.
- Always return some content when input is non-empty.
- Add a clear marker/header in the truncated stream when truncation happens.
- Add a `[tau-shell] warning: ...` in stderr when stdout and/or stderr was truncated, so the agent has an explicit signal outside the stream marker.
- Add regression tests for one huge line, huge final line, and no invalid UTF-8 slicing.

### 6. Truncation metadata is misleading

Status: `done`

Done note: shell details keep returned-content stats and add original total/truncated fields only for streams that were actually truncated, with regression coverage for absent and present metadata.

Problem:

- `stdout_lines`, `stdout_bytes`, `stderr_lines`, and `stderr_bytes` are counted from truncated content, not original output.

Agreed plan:

- Keep existing fields as returned-content stats for compatibility.
- Only when truncation actually happens, add:
  - `stdout_total_lines`, `stdout_total_bytes`, `stdout_truncated`
  - `stderr_total_lines`, `stderr_total_bytes`, `stderr_truncated`
- Clarify tests/docs.

### 7. Comment says full failure output is preserved, but it is not

Status: `done`

Done note: shell failure comment now says stdout/stderr details are preserved subject to truncation.

Plan:

- Fix the code comment to say stdout/stderr details are preserved subject to truncation.
- If issue 6 adds truncation metadata, mention how to detect truncation.

### 8. Non-zero exit is a `ToolError`, but description implies normal result

Status: `done`

Done note: shell and gpt_shell descriptions are concise and mention non-zero exits/timeouts as tool errors with stdout/stderr details.

Plan:

- Shorten and correct the model-visible shell description.
- Mention that non-zero exits and timeouts are returned as tool errors with stdout/stderr details.
- Do not make the description long.

### 9. Signal deaths lose useful status information

Status: `done`

Done note: Unix signal terminations now report `signal`, use `termination_reason: "signal"`, and error as `command terminated by signal N`, with regression coverage.

Agreed plan:

- On Unix, use `std::os::unix::process::ExitStatusExt::signal()` and expose `signal` in details.
- Improve message to `command terminated by signal 15` or equivalent.
- For non-Unix, keep generic unknown termination reason.
- Add regression test using `kill -TERM $$`.

### 10. Timeout errors omit structured timeout/status fields

Status: `done`

Done note: command details now include `timed_out`, `timeout_secs`, and `termination_reason` without adding elapsed timing.

Agreed plan:

- Add `timed_out: bool` to details.
- Add `timeout_secs` to details when relevant.
- Add `termination_reason` string such as `exit`, `signal`, or `timeout`.
- Do not add `elapsed_ms` now.

### 11. `cwd` is implemented but not exposed in schema

Status: `done`

Done note: shell and gpt_shell schemas now expose optional `cwd`, with schema and execution-path tests.

Agreed plan:

- Add `cwd` to the `shell` and `gpt_shell` schemas.
- Document it as optional working directory.
- Add test that schema exposes `cwd`, and existing execution path still works.

### 12. `timeout` validation is too silent

Status: `done`

Done note: timeout schemas require minimum 0; `0` is accepted as immediate timeout, while negative and wrong-type values return tool errors.

Agreed plan:

- Add schema `minimum: 0`.
- Accept `0` as immediate timeout.
- Reject negative timeout with a tool error.
- Reject wrong-type timeout with a tool error instead of silently using default.
- Add tests for `0`, negative, and wrong type.

### 13. stdout/stderr ordering is not preserved

Status: `wontfix`

Rationale:

- Separate stdout/stderr fields are acceptable.
- Chronological interleaving is not required for the shell tool right now.

### 14. Output is fully accumulated before truncation

Status: `done`

Done note: shell-tool pipe capture now keeps bounded per-stream tail buffers while separately counting original decoded bytes/lines for truncation markers and metadata.

Agreed plan:

- Improve together with issue 2.
- While rewriting pipe handling, use bounded per-stream tail buffers so memory is limited to around the truncation cap plus accounting.
- Count total bytes/lines separately from retained tail output.
- Do not separately refactor before the timeout/read-loop redesign.
