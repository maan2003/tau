---
name: tau-self-knowledge-debugging
description: >
  Use this skill when debugging Tau runs, daemons, runtime behavior, socket
  attachment, replay, logs, provider requests, token/cache usage, event ordering,
  or persisted state under Tau config, state, agent, debug, and runtime directories.
advertise: false
---

## Important paths

Tau follows the XDG directories:

- Config: `~/.config/tau/`
  - `cli.yaml`, `cli.d/*.yaml` — CLI display and key-binding config.
  - `harness.yaml`, `harness.d/*.yaml` — harness, agent roles/defaults, extensions, tools, and agent/debug-retention config.
- State: `~/.local/state/tau/` on Linux.
  - If no XDG state dir is available, inspection defaults may fall back to `.tau/state`.
  - `cli.json` — persisted CLI runtime toggles such as show-diff, show-thinking, show-tools, turn stats.
  - `policy.cbor` — persisted socket-client policy approvals.
  - `auth.d/<provider>.json` — per-provider credentials.
  - `auth.json` — legacy whole-file credentials, read for backwards compatibility.
- Agents: `~/.local/state/tau/agents/<agent_id>/`
  - `events.cbor` — durable agent transcript log and source of truth for replaying that agent tree.
  - `meta.json` — agent metadata such as cwd, creation time, and latest prompt preview.
  - `lock` — flock used while the daemon has the agent loaded for writing.
- Debug runs: `~/.local/state/tau/debug/<run_id>/`
  - `events.jsonl` — append-only harness runtime event log for the run. It mirrors committed bus events and is not authoritative replay state.
  - `provider-requests/*-{request,response}.json` — exact upstream Responses request bodies plus parsed/provider-terminal response captures written by provider extensions, keyed by timestamp, `agent_prompt_id`, and transport. These include full prompt content, tool results, and model outputs, but not auth headers/API keys.
  - `logs/<extension>.log` — stderr for each spawned extension.
- Runtime: `${XDG_RUNTIME_DIR}/tau/harnesses/` or `/tmp/tau-$USER/harnesses/`
  - `<pid>.sock` — Unix socket for clients.
  - `<pid>.json` — daemon discovery metadata, including project root and pid.

## Event logs are usually the first place to look

For runtime, UI, or daemon misbehavior, inspect the current run's `events.jsonl`
early. It is append-only JSONL meant for post-mortems and contains the
harness-level event stream, including transient events that are not in durable
agent replay. This makes it better than semantic `events.cbor` files when
debugging missing UI updates, streaming updates, tool progress, connection
churn, ordering issues, or short-lived states.

Each debug log line includes fields such as:

- `type` — commonly `from_connection`, `published`, `disconnected`, or `new_client`.
- `recorded_at_micros` — timestamp useful for ordering and latency gaps.
- `source` — connection id when known.
- `event_name` — protocol event name.
- `event` — compacted event payload.

Use agent `events.cbor` when debugging transcript/tree reconstruction. Use
debug-run `events.jsonl` when debugging runtime behavior.

## Drive a running daemon

Use `cargo r -- dev send <line...>` to inject user-equivalent input into a
running daemon. This is useful for agent-powered debugging because it goes
through the socket protocol and normal UI event path instead of editing
persisted logs by hand.

Examples:

```bash
cargo r -- dev send "normal user message"
cargo r -- dev send /cancel
cargo r -- dev send /model smart
cargo r -- dev send /compact
cargo r -- dev send '!pwd'
```

The command connects to a running harness socket. It supports normal prompts,
core slash commands, and `!` / `!!` shell-command submissions.

## Quick inspection workflow

1. Identify the current run id from the `harness.started` event, or list recent
   `~/.local/state/tau/debug/` directories and sort by `events.jsonl` mtime.
2. Read `events.jsonl` around the failing prompt first.
3. Cross-check extension logs under `logs/` for errors or panics.
4. Check agent `events.cbor` only when the bug involves replay or persisted semantic contents.
5. Check runtime daemon files under `${XDG_RUNTIME_DIR}/tau/harnesses/` when the bug involves attach, wrong project daemon selection, or socket connection failures.
6. For provider/cache-shape bugs, inspect `provider-requests/` for the exact request body Tau sent upstream and the response capture it parsed afterward.

Helpful commands:

```bash
# Pretty-print recent debug events for one run.
tail -n 200 ~/.local/state/tau/debug/<run_id>/events.jsonl | jq .

# Find recent debug run directories by event-log mtime.
find ~/.local/state/tau/debug -maxdepth 2 -name events.jsonl -printf '%T@ %h\n' | sort -n

# Inspect extension logs for one run.
ls -lah ~/.local/state/tau/debug/<run_id>/logs

# Inspect exact provider request/response captures, if present.
ls -lah ~/.local/state/tau/debug/<run_id>/provider-requests
jq '.previous_response, .body.previous_response_id, .body.input' ~/.local/state/tau/debug/<run_id>/provider-requests/*-sp-6-*-request.json
jq '.response_id, .cached_tokens, .provider_terminal_event.response.usage, .agent_response_finished.tool_calls' ~/.local/state/tau/debug/<run_id>/provider-requests/*-sp-6-*-response.json
```

## Token/cache efficiency analysis

When asked to analyze cache hit or token usage efficiency for a Tau run,
inspect `events.jsonl` and count `provider.response_finished` events. These
events often appear twice: once with `type: "from_connection"` and once with
`type: "published"`. Filter to one type, preferably `from_connection`, or
dedupe by `(response_id, agent_prompt_id)` to avoid exactly doubling token
totals.

Useful one-shot summary:

```bash
python3 - <<'PY'
import json, pathlib
run_id = '<run_id>'
p = pathlib.Path.home() / '.local/state/tau/debug' / run_id / 'events.jsonl'
rows = []
for ln, line in enumerate(p.open(), 1):
    j = json.loads(line)
    ev = j.get('event', {})
    if ev.get('event') == 'provider.response_finished' and j.get('type') == 'from_connection':
        pl = ev.get('payload', {})
        usage = pl.get('usage') or {}
        sp = pl.get('agent_prompt_id') or '?'
        inp = usage.get('prompt_sent_tokens') or pl.get('input_tokens') or 0
        cached = usage.get('prompt_cached_tokens') or pl.get('cached_tokens') or 0
        out = usage.get('response_received_tokens') or pl.get('output_tokens') or 0
        rows.append((sp, ln, inp, cached, inp - cached, out, pl.get('originator')))

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

Red flags found in past runs:

- Internal extension prompts, especially `std-notifications` idle summaries, can create normal `ui.prompt_submitted` / `agent.prompt_created` / `provider.prompt_submitted` sequences with originator `{kind: "extension"}`. If they resend full history, cache continuity may collapse and waste many uncached tokens for tiny outputs. Check lines around `agent.start_request`, `ui.prompt_submitted`, and the following `provider.response_finished`.
- `harness.context_usage_changed` currently follows all `provider.response_finished` events, including extension-originated prompts. Treat context/token stats carefully if side-channel prompts are present.
- Large tool outputs in `agent.prompt_created` messages can dominate context: repeated large `read` slices, cargo/check output, clippy output, or colorized `jj diff`. Grep for `┄total <n>┄` markers in `events.jsonl` to find compacted large payloads.
- For exact, uncompacted provider payloads, check `provider-requests/*-{request,response}.json`. Request files are especially useful for cache misses involving `previous_response_id`, multi-tool-call suffixes, tool-use/tool-result ordering, or mismatches between `agent.prompt_created` and the serialized upstream `body.input`; response files show Tau's parsed `provider.response_finished` shape plus the raw terminal provider event (`response.completed` / `response.done`) when available.
- Repeated `provider.response_updated` streaming events are numerous and not useful for aggregate token accounting. Prefer `provider.response_finished`.

Quick checks for side-channel waste:

```bash
# Show extension-originated prompt/response activity.
grep -n 'agent.start_request\|std-notifications\|"kind":"extension"' ~/.local/state/tau/debug/<run_id>/events.jsonl

# Search logs for runtime errors; no matches does not rule out token waste.
grep -RniE 'error|warn|panic|cache|token' ~/.local/state/tau/debug/<run_id>/logs
```
