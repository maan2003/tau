# Event log reference

The tau bus is fact-based: components broadcast what happened, never requests
or replies. Every event has a dotted name `<category>.<call>` and a typed
payload defined in `crates/tau-proto/src/events.rs`. This document groups the
core events by the component (or class of component) that emits them.

Events are distinct from **messages**: messages are point-to-point control-plane
traffic (handshake, subscribe/intercept, the `LogEvent`/`Ack` envelope, etc.)
and never appear on the bus or in the durable session log. See
[messages.md](messages.md) for the message-side reference.

A few categories don't map to a single emitter — those are grouped by the
class of function that raises them.

## Harness (general)

Emitted by the harness daemon itself, mostly for UI-facing status and
for control of the emit/intercept pipeline.

- **`harness.info`** — A free-form informational message from the
  harness for the user, with a severity (`normal` / `important`). Used
  for things like `/tree` rendering and ad-hoc notices.
- **`harness.models_available`** — The full provider-published model list
  as `provider/model_id` strings. Re-emitted when provider snapshots change.
- **`harness.role_selected`** — Which role is currently selected, plus
  the model it resolves to and that model's context-window size if known.
- **`harness.context_usage_changed`** — Updated input/cached token counts
  and percent-of-context-window for the selected role's resolved model,
  after each agent response that reports usage.
- **`harness.effort_changed`** — The current reasoning-effort level
  (`off` / `minimal` / `low` / `medium` / `high` / `xhigh`).
- **`harness.service_tier_changed`** — The current service tier (`fast`,
  `flex`, or absent to use provider default).
- **`harness.efforts_available`** — Which effort levels are valid for the
  selected role's resolved model. Empty when the selected role has no
  resolved model or the provider doesn't support reasoning.

## Session (harness session tracker)

Emitted by the harness's session tracker. They drive the durable session
tree and the prompt lifecycle.

- **`session.started`** — A session was created or switched to. Carries
  a reason (`initial` startup, `new` via `/session new`, `resume` of an
  existing session). Extensions react with per-session setup and reply
  with `extension.context_ready`.
- **`session.shutdown`** — The harness is leaving the current session,
  emitted before `session.started` for the next one. Extensions flush or
  drop per-session state.
- **`session.prompt_queued`** — A user prompt arrived while the agent
  was busy and was queued instead of dispatched. Operational only;
  transient rather than durable transcript state.
- **`session.prompt_steered`** — A previously queued prompt is being
  folded into the in-flight turn as a steering message rather than
  starting a fresh turn. Folds into the session tree as one user
  message at the current head.
- **`session.prompt_created`** — The harness persisted a prompt and
  assigned it an id; payload carries `system_prompt`, assembled
  `context_items`, available tools, model, effort,
  thinking-summary setting, and originator. This is the input handed
  to the selected provider. `context_items` is fully materialized history for the
  turn. This event is transient operational delivery state, not durable
  transcript truth. `tools_ref`, when present, means
  `tools` is empty; copy full tool definitions from the referenced
  prompt. Durable transcript truth comes from later
  user/assistant/completed-tool/session-compacted facts.
- **`session.user_message_injected`** — A synthetic user message
  inserted by the harness (e.g. `!`-shell command output, AGENTS.md
  preamble). Folds into the session tree like a real user prompt.

## Provider execution

Emitted by the provider backend that owns the selected model.

- **`provider.prompt_submitted`** — The provider accepted a `session.prompt_created`
  and started processing it. Echoes the originator. Transient.
- **`provider.response_updated`** — Streaming update with the full text so
  far (replace, not delta) and accumulated reasoning summary if any.
  Transient by default.
- **`provider.response_finished`** — Final assistant output in original
  item order via `output_items`, plus optional usage, provider
  response id, backend metadata, and echoed originator. Routed by the
  harness based on the originator.
- **`provider.tool_result`** / **`provider.tool_error`** — Provider-facing
  terminal tool-call completions. These satisfy provider protocol state and
  fold into prompt history, but are not logical UI tool completions. The
  synthetic background placeholder uses `provider.tool_result` only.

## Tools

Tool events span three emitters: extensions register/implement tools,
the agent requests calls, and the harness orchestrates dispatch.

- **`tool.register`** *(extension)* — A tool provider advertises a tool
  spec (name, description, JSON-schema parameters, `enabled_by_default`,
  execution mode).
- **`tool.unregister`** *(extension)* — A previously registered tool is
  withdrawn.
- **`tool.request`** *(provider/extension)* — A request to run a
  tool call by id, model-produced name, and CBOR arguments. It may come from
  an agent response or another extension, and can still be rejected before any
  provider receives it. Persisted as the pre-routing intent.
- **`tool.started`** *(harness)* — The harness accepted and routed a
  tool request. This durable broadcast is the signal that the selected tool
  provider should start the call, and that UIs can show the tool as running.
- **`tool.rejected`** *(harness)* — The harness rejected a tool request
  before any tool provider was asked to run it. UIs can display this as a tool
  call rejection.
- **`tool.result`** *(extension/harness)* — Successful logical runtime tool
  completion, by call id, with tool-owned `result` plus optional UI
  `display` metadata and echoed originator. This event is renderer-facing.
  Provider-only terminal completions use `provider.tool_result` instead.
- **`tool.error`** *(extension)* — Logical tool failure with a message and
  optional structured details. Operational only; transient. Provider-only
  terminal failures use `provider.tool_error` instead.
- **`tool.background_result`** / **`tool.background_error`** *(harness)* —
  Logical notification that a backgrounded tool later completed for real.
  The earlier synthetic placeholder is provider-facing only and is not
  emitted as `tool.result`.
- **`tool.progress`** *(extension)* — In-flight progress update with an
  optional message and current/total counters. Transient.
- **`tool.cancel`** *(harness)* — The harness asks an extension to
  cancel an in-flight call.
- **`tool.cancelled`** *(extension)* — The extension acknowledges that a
  call has been cancelled. Operational only; transient.
- **`tool.delegate_progress`** *(harness)* — Live snapshot of a sub-agent
  spawned by the `delegate` tool: task name, resolved delegate role,
  tools-in-flight, total, context tokens, percent. Transient; the UI
  re-renders the parent tool block.

## Extensions

Two sub-classes:

### Extension supervision (harness supervisor)

Emitted by the harness's supervisor as it manages child extension
processes.

- **`extension.starting`** — A child extension process is being spawned
  (instance id, name, pid).
- **`extension.ready`** — The extension's `Ready` message was received
  by the supervisor, which synthesizes this bus event so subscribers can
  observe that the extension is fully online.
- **`extension.exited`** — The child process exited; carries exit code
  and/or signal.
- **`extension.restarting`** — The supervisor is restarting an extension
  (attempt counter, optional reason).

### Extension-emitted

Emitted by extensions to advertise capabilities or interact with the
harness/agent.

- **`extension.skill_available`** — The extension discovered a skill on
  disk: name, description, file path, and whether to inject it into the
  system prompt.
- **`extension.agents_md_available`** — The extension discovered an
  AGENTS.md file and is shipping its contents eagerly so the harness
  can inject them without a tool round-trip.
- **`extension.context_ready`** — The extension finished publishing
  refreshed prompt context for one session (the reply to
  `session.started`).
- **`agent.start_request`** — An extension or harness-owned tool asks
  the harness to start a side/sub-agent conversation: instruction text,
  correlation `query_id`, optional requested `role`, optional tool-call
  attribution, and human-readable task name (used by the `delegate` tool).
  Tool-backed delegate requests default to `engineer` when `role` is
  absent; non-tool tool requests without `role` use the currently selected
  interactive role.
- **`agent.start_result`** — The agent's final answer to an
  earlier `agent.start_request`, routed point-to-point back to the
  requesting extension. Carries the same `query_id`.
- **`agent.message`** — An agent sends a short text message to another
  agent or to the user. Carries `session_id`, `sender_id`,
  `recipient_id`, and `message`. This is durable transcript state, so
  late UI subscribers can replay it. UI subscribers filter, summarize, or
  fully display messages according to `/set show-messages`. A
  `recipient_id` of `user` only displays the message. Any other recipient
  is resolved to a live or pending agent conversation and delivered as a
  hidden internal prompt; if that agent is a side/delegate agent about to
  finish, teardown waits until the message turn has been dispatched and
  answered. See [agent-messaging.md](agent-messaging.md) for model-facing
  tool examples.
- **`extension.event`** — Custom extension-defined event with a free-form
  dotted name and CBOR payload. The harness routes it like any other
  event; if `session_id` is set it can be folded into that session's
  durable log.

## UI

Emitted by attached UI clients (tau-cli-term, etc.) to express user
intent.

- **`ui.prompt_submitted`** — The user submitted a prompt: session id,
  text, originator (defaults to `user`; reused for extension-driven
  side prompts).
- **`ui.prompt_draft`** — Trailing-edge debounced (≤1/s) snapshot of the
  current draft buffer. Transient — used for "user is alive" signals
  (e.g. notification idle reset), not persisted.
- **`ui.role_select`** — User requests a role switch. The harness resolves
  the role to a provider-published model at runtime.
- **`ui.role_update`** — User changes or deletes a role. Updates are typed
  field mutations (`model`, `effort`, `verbosity`, `thinking-summary`,
  `service-tier`, `tools-profile`) and are persisted as runtime role
  overrides; omitted/null fields clear the role value back to
  model/provider fallback behavior.
- **`ui.detach_request`** — UI is detaching but wants the daemon to keep
  running so a later `tau --attach` can reconnect.
- **`ui.shell_command`** — User submitted a `!` (in-context) or `!!`
  (UI-only) shell command. Carries command id, command, session id,
  `include_in_context` flag.
- **`ui.switch_session`** — User wants to switch to a different session
  in the same daemon, with `new`/`resume` reason.
- **`ui.tree_request`** — User typed `/tree`: render the session
  branching tree to chat.
- **`ui.navigate_tree`** — User typed `/tree <id>`: move the session
  head to that node so the next prompt branches off there.

## Shell (shell extension, user-initiated commands)

Emitted by `tau-ext-shell` (or any extension implementing `!`/`!!`
commands) in response to a `ui.shell_command`.

- **`shell.command_progress`** — A chunk of stdout/stderr from a running
  user-initiated shell command, correlated by `command_id`. Transient.
- **`shell.command_finished`** — A user-initiated shell command exited
  or was cancelled. Echoes session id, command, and `include_in_context`
  flag from the originating request, plus the truncated combined
  output, exit code, and `cancelled` flag.

## Term (terminal-output side effects)

Targeted at whichever UI is attached and capable of writing escape
sequences to a real terminal. Any component may emit these; the UI is
the only consumer. Components without a terminal silently no-op.

- **`term.osc1337_set_user_var`** — Ask the UI to write an iTerm2
  OSC 1337 `SetUserVar` escape sequence. The UI base64-encodes the
  value and tmux-wraps if needed. Useful for surfacing notifications,
  build status, or other state to terminal-side tooling.
