# tau-ext-std-notifications

A tau extension that bridges agent activity into iTerm2-style
[OSC 1337 `SetUserVar`][osc1337] user-variable updates. Mirrors the
shape of dpc's Pi extensions
[`notification-sounds.ts`][pi-sounds] and
[`idle-notification.ts`][pi-idle], adapted to tau's harness-mediated
event bus.

The extension itself does not play sounds or pop notifications. It
only writes user-vars; downstream tooling — typically a terminal
multiplexer status line, or shell scripts like
`user-notification.sh` / `user-text-notification.sh` watching for
those user-vars — is what actually plays a sound or fires a desktop
notification.

## What it does

Three classes of event are emitted via the
[`term.osc1337_set_user_var`][var] protocol event:

| Trigger | User-var | Value |
|---|---|---|
| `ui.prompt_submitted` (originator: User) | `user-notification` | `protoss-probe-ack` |
| Final `provider.response_finished` (no pending tool calls, originator: User) | `user-notification` | `protoss-upgrade-complete` |
| Idle window elapses after a final response | `user-text-notification` | JSON payload (see below) |

The "final response" filter only treats responses with `tool_calls`
empty as the end of an agent turn. Mid-turn finishes (tool-call
batches the harness will run, then re-prompt) are skipped so the
end-of-turn sound only fires once per real turn.

The `originator: User` filter ensures *side conversations* spawned
by other extensions (or by this one — see below) do not retrigger
the sounds or perturb the idle state machine.

## Idle text notification

After `idle_seconds` (default 60) of inactivity following a final
agent response, the extension fires the `user-text-notification`
user-var with the static "Waiting for user input" body.

If `idle_agent_summary` is set to `true`, the extension uses the old
summary path instead:

1. The extension sends an `extension.agent_query` to the harness
   asking the agent for a one-sentence summary of the conversation
   (instruction is hard-coded; matches Pi's wording).
2. The harness spawns a side conversation off the user's current
   branch, dispatches the prompt to the agent, and routes the
   matching `extension.agent_query_result` back point-to-point.
3. The extension fires the `user-text-notification` user-var with
   the model's summary as the body. If the result doesn't arrive
   within `SUMMARY_TIMEOUT_SECONDS` (10s) — wedged agent, no
   model, etc. — it falls back to the static "Waiting for user
   input" body so the user always gets nudged.

The idle deadline resets on:

- `ui.prompt_submitted` (originator: User) — the user just sent a
  prompt;
- `provider.prompt_submitted` (originator: User) — the provider is
  starting a real turn;
- `ui.prompt_draft` — trailing-edge debounced typing pings from
  the UI; the deadline jumps back by `idle_seconds` so the
  notification doesn't fire mid-sentence while the user is
  composing. Only applies in the `WaitingIdle` state; an
  in-flight side-query summary (`WaitingSummary`, only possible when
  `idle_agent_summary` is enabled) is left alone because we don't
  currently have a way to cancel the agent's in-flight prompt without
  billing for it.

## Text-notification payload schema

The `user-text-notification` user-var carries a JSON object that
matches what `user-text-notification.sh` emits, so the same
downstream consumers handle both sources:

```json
{
  "urgency": "normal",
  "title":   "Agent idle: <hostname>:<basename(cwd)>",
  "body":    "<model summary, or static fallback>",
  "app_name": "tau"
}
```

`app_name` lets desktop-notification consumers (libnotify et al.)
group, route, or style tau notifications distinctly without us
having to bake "tau" into the title text.

## Configuration

The extension reads its config from the `extensions.<name>.config`
field of `harness.json5`. All fields are optional; unknown fields
are rejected with a `lifecycle.config_error` so the harness can
surface typos to the user.

```json5
{
  extensions: {
    "std-notifications": {
      enable: true,
      config: {
        // Idle window (seconds) before the extension nudges the
        // user. Default: 60.
        idle_seconds: 60,

        // Ask the agent for a one-sentence idle summary before
        // notifying. Default: false; the default notification body is
        // the static "Waiting for user input" text.
        idle_agent_summary: false,

        // Optional argv to invoke when the text notification
        // would normally fire (idle summary or fallback). The
        // command runs *in addition to* the OSC user-var write,
        // never instead of it, so existing terminal-side
        // consumers keep working.
        idle_command: ["user-text-notification.sh"],
      },
    },
  },
}
```

### `idle_command` calling convention

Mirrors `user-text-notification.sh` so the script itself — or
anything that follows the same shape — can be plugged in directly:

- `argv[0]` is the program; any extra elements you put in the
  array are passed as additional arguments *before* the title.
- The notification **title** is appended as the next argument.
- The notification **body** is piped to the command's stdin.
- `NOTIFY_URGENCY=normal` and `NOTIFY_APP_NAME=tau` are set in the
  child's environment.

The command runs detached on a worker thread; stdout and stderr
are discarded, and a non-zero exit logs at `warn` but is otherwise
ignored. The main extension loop is never blocked on it.

Examples:

```json5
// Plug in user-text-notification.sh directly.
idle_command: ["user-text-notification.sh"]

// Wrap in a custom dispatcher with extra args before the title.
idle_command: ["my-notify.sh", "--channel", "tau-agent"]

// Use anything that reads stdin: notify-send needs the body as an
// arg, so wrap it in a shell:
idle_command: [
  "bash", "-c",
  "body=$(cat); notify-send --app-name=tau \"$1\" \"$body\"",
  "_tau",
]
```

## Tracing

The extension uses the `std-notifications` tracing target:

```sh
TAU_EXT_LOG=std-notifications=debug tau …
```

`debug` shows `received ExtAgentQueryResult { idle_state, query_id,
text_len, error }` and idle-deadline transitions; `trace` adds one
line per ignored event for protocol-level debugging.

[osc1337]: https://iterm2.com/documentation-escape-codes.html
[pi-sounds]: https://github.com/dpc/dpc-personal/blob/master/.pi/agent/extensions/notification-sounds.ts
[pi-idle]: https://github.com/dpc/dpc-personal/blob/master/.pi/agent/extensions/idle-notification.ts
[var]: ../tau-proto/src/events.rs
