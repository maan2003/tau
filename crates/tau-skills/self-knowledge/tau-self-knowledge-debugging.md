---
name: tau-self-knowledge-debugging
description: Debug Tau sessions and daemon behavior by inspecting Tau state, session stores, event logs, and runtime files.
advertise: false
---

## Important paths

Tau follows the XDG directories:

- Config: `~/.config/tau/`
  - `cli.json5`, `cli.d/*.json5` — CLI display and key-binding config.
  - `harness.json5`, `harness.d/*.json5` — harness, agent roles/defaults, extension, and session-retention config.
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
  - `debug/provider-requests/*-{request,response}.json` — exact upstream Responses request bodies plus parsed/provider-terminal response captures written by provider extensions, keyed by timestamp, `session_prompt_id`, and transport. These include full prompt content, tool results, and model outputs, but not auth headers/API keys.
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
6. For provider/cache-shape bugs, inspect `debug/provider-requests/` for the exact request body Tau sent upstream and the response capture it parsed afterward.

Helpful commands:

```bash
# Pretty-print recent debug events for one session.
tail -n 200 ~/.local/state/tau/sessions/<session_id>/events.jsonl | jq .

# Find recent session directories.
find ~/.local/state/tau/sessions -maxdepth 1 -mindepth 1 -type d -printf '%T@ %p\n' | sort -n

# Inspect logs for one session.
ls -lah ~/.local/state/tau/sessions/<session_id>/logs

# Inspect exact provider request/response captures, if present.
ls -lah ~/.local/state/tau/sessions/<session_id>/debug/provider-requests
jq '.previous_response, .body.previous_response_id, .body.input' ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*-sp-6-*-request.json
jq '.response_id, .cached_tokens, .provider_terminal_event.response.usage, .agent_response_finished.tool_calls' ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*-sp-6-*-response.json
```


## Token/cache efficiency analysis

When asked to analyze cache hit or token usage efficiency for a session, inspect `events.jsonl` and count `provider.response_finished` events. These events often appear twice: once with `type: "from_connection"` and once with `type: "published"`. Filter to one type, preferably `from_connection`, or dedupe by `(response_id, session_prompt_id)` to avoid exactly doubling token totals.

Useful one-shot summary:

```bash
python3 - <<'PY'
import json, pathlib
sid = '<session_id>'
p = pathlib.Path.home() / '.local/state/tau/sessions' / sid / 'events.jsonl'
rows = []
for ln, line in enumerate(p.open(), 1):
    j = json.loads(line)
    ev = j.get('event', {})
    if ev.get('event') == 'provider.response_finished' and j.get('type') == 'from_connection':
        pl = ev.get('payload', {})
        sp = pl.get('session_prompt_id') or '?'
        inp = pl.get('input_tokens') or 0
        cached = pl.get('cached_tokens') or 0
        rows.append((sp, ln, inp, cached, inp - cached, pl.get('output_tokens') or 0, pl.get('originator')))

for label, subset in [('all', rows), ('user', [r for r in rows if (r[6] or {}).get('kind') == 'user']), ('extension', [r for r in rows if (r[6] or {}).get('kind') == 'extension'])]:
    total_in = sum(r[2] for r in subset)
    total_cached = sum(r[3] for r in subset)
    total_uncached = sum(r[4] for r in subset)
    total_out = sum(r[5] for r in subset)
    pct = 100 * total_cached / total_in if total_in else 0
    print(label, 'calls', len(subset), 'input', total_in, 'cached', total_cached, 'uncached', total_uncached, 'cache_pct', round(pct, 1), 'output', total_out)

print('\nlargest uncached calls:')
for sp, ln, inp, cached, uncached, out, origin in sorted(rows, key=lambda r: r[4], reverse=True)[:10]:
    pct = 100 * cached / inp if inp else 0
    print(sp, 'line', ln, 'input', inp, 'cached', cached, 'uncached', uncached, 'cache_pct', round(pct, 1), 'output', out, 'origin', origin)
PY
```

Red flags found in past sessions:

- Internal extension prompts, especially `std-notifications` idle summaries, can create normal `ui.prompt_submitted` / `session.prompt_created` / `provider.prompt_submitted` sequences with originator `{kind: "extension"}`. If they resend full history, cache continuity may collapse and waste many uncached tokens for tiny outputs. Check lines around `extension.agent_query`, `ui.prompt_submitted`, and the following `provider.response_finished`.
- `harness.context_usage_changed` currently follows all `provider.response_finished` events, including extension-originated prompts. Treat context/token stats carefully if side-channel prompts are present.
- Large tool outputs in `session.prompt_created` messages can dominate context: repeated large `read` slices, cargo/check output, clippy output, or colorized `jj diff`. Grep for `┄total <n>┄` markers in `events.jsonl` to find compacted large payloads.
- For exact, uncompacted provider payloads, check `debug/provider-requests/*-{request,response}.json`. Request files are especially useful for cache misses involving `previous_response_id`, multi-tool-call suffixes, tool-use/tool-result ordering, or mismatches between `session.prompt_created` and the serialized upstream `body.input`; response files show Tau's parsed `provider.response_finished` shape plus the raw terminal provider event (`response.completed` / `response.done`) when available.
- Repeated `provider.response_updated` streaming events are numerous and not useful for aggregate token accounting. Prefer `provider.response_finished`.

Quick checks for side-channel waste:

```bash
# Show extension-originated prompt/response activity.
grep -n 'extension.agent_query\|std-notifications\|"kind":"extension"' ~/.local/state/tau/sessions/<session_id>/events.jsonl

# Search logs for runtime errors; no matches does not rule out token waste.
grep -RniE 'error|warn|panic|cache|token' ~/.local/state/tau/sessions/<session_id>/logs
```
