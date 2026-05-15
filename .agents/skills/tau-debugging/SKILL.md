---
name: tau-debugging
description: "Debug Tau sessions and daemon behavior by inspecting Tau state, session stores, event logs, and runtime files."
user-invocable: true
advertise: true
---

## Important paths

Tau follows the XDG directories:

- Config: `~/.config/tau/`
  - `cli.json5`, `cli.d/*.json5` — CLI display and key-binding config.
  - `harness.json5`, `harness.d/*.json5` — harness, agent, extension, and session-retention config.
  - `models.json5`, `models.d/*.json5` — provider and model registry config.
- State: `~/.local/state/tau/` on Linux.
  - If no XDG state dir is available, inspection defaults may fall back to `.tau/state`.
  - `cli.json` — persisted CLI runtime toggles such as show-diff, show-thinking, show-tools, token stats.
  - `policy.cbor` — persisted socket-client policy approvals.
  - `auth.d/<provider>.json` — per-provider credentials.
  - `auth.json` — legacy whole-file credentials, read for backwards compatibility.
- Sessions: `~/.local/state/tau/sessions/<session_id>/`
  - `events.cbor` — durable per-session protocol event log. This is the source of truth for replaying the session tree.
  - `meta.json` — session metadata such as cwd, creation time, and last-touched time.
  - `lock` — flock used while the daemon has the session loaded for writing.
  - `events.jsonl` — debug event log for the session.
  - `logs/tau-harness.log` — harness daemon stderr/tracing for the session.
  - `logs/<extension>.log` — stderr for each spawned extension.
- Runtime: `${XDG_RUNTIME_DIR}/tau/<pid>/` or `/tmp/tau-$USER/<pid>/`
  - `tau.sock` — Unix socket for clients.
  - `tau.dir` — project root marker used for daemon discovery.
  - `tau.pid` — daemon process id.
  - `tau.session_id` — session id bound to the daemon.

## Event logs are usually the first place to look

For session misbehavior, inspect `~/.local/state/tau/sessions/<session_id>/events.jsonl` early. It is append-only JSONL meant for post-mortems and contains the harness-level event stream, including transient events that are not in durable session replay. This makes it better than `events.cbor` when debugging missing UI updates, streaming updates, tool progress, connection churn, ordering issues, or short-lived states.

Each debug log line includes fields such as:

- `type` — commonly `from_connection`, `published`, `disconnected`, or `new_client`.
- `recorded_at_micros` — timestamp useful for ordering and latency gaps.
- `source` — connection id when known.
- `event_name` — protocol event name.
- `event` — compacted event payload.

Use the durable `events.cbor` when debugging replay, persistence, or session-tree reconstruction. Use `events.jsonl` when debugging runtime behavior.

## Drive a running session

Use `cargo r -- dev send <session_id> <line...>` to inject user-equivalent input into a running daemon-bound session. This is useful for agent-powered debugging because it goes through the socket protocol and normal UI event path instead of editing persisted logs by hand.

Examples:

```bash
cargo r -- dev send <session_id> "normal user message"
cargo r -- dev send <session_id> /cancel
cargo r -- dev send <session_id> /model smart
cargo r -- dev send <session_id> /compact
cargo r -- dev send <session_id> '!pwd'
```

The command requires the session id and finds the matching running daemon via its runtime `tau.session_id` marker. It supports normal prompts, core slash commands, and `!` / `!!` shell-command submissions.

## Quick inspection workflow

1. Identify the session id. If unsure, list `~/.local/state/tau/sessions/` and sort by `meta.json` or directory mtime.
2. Read `events.jsonl` around the failing prompt first.
3. Cross-check with `logs/tau-harness.log` and extension logs for errors or panics.
4. Check `events.cbor` only when the bug involves replay or persisted session contents.
5. Check runtime daemon files under `${XDG_RUNTIME_DIR}/tau/` when the bug involves attach/resume, wrong project daemon selection, or socket connection failures.

Helpful commands:

```bash
# Pretty-print recent debug events for one session.
tail -n 200 ~/.local/state/tau/sessions/<session_id>/events.jsonl | jq .

# Find recent session directories.
find ~/.local/state/tau/sessions -maxdepth 1 -mindepth 1 -type d -printf '%T@ %p\n' | sort -n

# Inspect logs for one session.
ls -lah ~/.local/state/tau/sessions/<session_id>/logs
```
