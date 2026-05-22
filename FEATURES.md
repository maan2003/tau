# Features

A guide to the major features of (dpc's) Tau coding agent. For high-level
philosophy and motivation see [README.md](README.md); for design notes see
[DESIGN.md](DESIGN.md) and [ARCHITECTURE.md](ARCHITECTURE.md).


## Architecture

### Process-oriented components

Every major component — UI, harness, LLM provider, extensions — runs as a
standalone POSIX process and talks CBOR-encoded events over stdio (extensions)
or a Unix socket (UI ↔ harness). A component is just an executable: supervise
it with your init system, sandbox it with bubblewrap or Landlock, swap it for
anything else that speaks the protocol, or write a new one in any language.

The default `tau` binary bundles all first-party components and dispatches via
hidden `tau ext <name>` subcommands; you can replace any of them by editing
`harness.yaml`.

### Persisted event log

Every protocol event in a session is appended to
`<state_dir>/sessions/<session_id>/events.cbor` (length-prefixed CBOR stream). The
in-memory [`SessionTree`] is rebuilt from the log on resume, so the on-disk
record and the live view cannot drift. Because the log is a stream of typed
events rather than a flat transcript, sessions branch into a tree: rewinding
to an earlier turn keeps the abandoned branch on disk.

```
$ tau session-list
$ tau session-show --session-id <id>
$ tau -r                  # pick a recent session for this cwd
$ tau -r <id>             # resume a specific one
```

Inside the UI, `/tree` prints the branch graph and `/tree <node-id>` rewinds
the head to that node.

### Interception system

Components can register as event interceptors with priority + selector pairs;
matching events are routed through the interceptor and only reach the bus and
event log if it allows them. Exact selectors win over prefix matches; ties are
broken by component name. This is how things like the policy gate or the
delegate progress tracker plug in without modifying the harness core.

See [`docs/interceptors.md`](docs/interceptors.md) and
`crates/tau-harness/src/interception.rs`.

### Remote extensions over SSH

Because extensions are stdio child processes, running one on another machine
is a matter of prefixing its argv with an `ssh` invocation:

```yaml
# harness.yaml
extensions:
  core-shell:
    prefix: ["ssh", "user@host"]
```

The harness prepends `prefix` to the resolved command. Anything that gives you
a stdio pipe to a remote process works the same way (`docker exec`, `nsenter`,
`bwrap`, …).

### Model parameters: effort, verbosity, thinking summary, service tier

Per-prompt knobs are bundled into a single `ModelParams` struct
that the harness stamps onto every `SessionPromptCreated` and that
backends thread through to the provider request:

- **`effort`** — reasoning effort. Six levels (`off`, `minimal`, `low`,
  `medium`, `high`, `xhigh`); maps to provider-specific reasoning controls.
  Provider extensions publish the exact effort levels each model accepts, and
  the harness clamps role/default selections to that published list.
- **`verbosity`** — output verbosity (`low`, `medium`, `high`).
  Sent to providers that advertise support (for ChatGPT/Codex Responses this is
  `text.verbosity`). Default `low` to keep model replies concise. Provider
  extensions publish the accepted verbosity levels per model.
- **`thinking_summary`** — reasoning-summary mode (`off`, `auto`,
  `concise`, `detailed`). Sent as `reasoning.summary` on providers
  that set `supportsReasoningSummary`; ignored otherwise.
- **`service_tier`** — optional upstream service tier. `/fast`
  toggles Codex's `fast` tier. Backends serialize Codex's exact
  OpenAI wire values: `priority` for Fast and `flex` for Flex.

Defaults are normally selected through agent roles in `harness.yaml`:

```yaml
promptFragments:
  - name: user.short-plain-style
    priority: 65
    text: Keep answers short and plain, using only simple words.

roles:
  engineer:
    description: Balanced coding assistant
    model: chatgpt/gpt-5.5
    effort: medium
    tools: [read, grep]
  assistant: { effort: off, serviceTier: fast }
  manager:
    promptFragments:
      - name: manager.workflow
        priority: 66
        text: Delegate non-trivial work.
```

Roles can include a `description` shown after the model/knob summary in
`/role ...` completions. Top-level `promptFragments` apply to every role;
per-role `promptFragments` apply only to that role. Roles can also use `tools`
and `disableTools` to customize internal tool availability.

`/model <role>` switches roles; `/role <role> <setting> <value>` edits role
settings, with built-in/configured role overrides persisted. See
[`docs/agent-roles.md`](docs/agent-roles.md).

In the UI: `/role engineer effort medium`, `/role engineer verbosity low`,
`/role engineer thinking-summary concise`. Tab cycles to the next agent role.
Model knobs are slash-command-only today. Asking for an unsupported
level (e.g. `effort xhigh` on a mini model, `verbosity high` on a provider
that doesn't support it) degrades and surfaces a `HarnessInfo` notice rather
than silently dropping the field.

The status bar renders only the selected agent role, falling back to the
model id when no role is selected. Model knobs and context usage stay out
of the bar to keep it compact.

### Prompt input caching

For providers that support it, Tau emits stable `prompt_cache_key` routing
keys (derived from base URL and session id) so cache hits survive restarts
within a session, and sets `prompt_cache_retention` where
available. Side-query turns (e.g. delegated sub-agents) get a distinct key so
parallel delegations don't pile onto the user agent's routing bucket and trip
OpenAI's `>15 RPM`-per-key overflow heuristic. Provider compatibility flags live
next to the model entry (`supports_prompt_cache_key`,
`supports_prompt_cache_retention`). Toggle the status-bar hit-rate readout with
`/set show-cache-stats <true|false>`.

### Policy / approvals

Subscription approvals are persisted to `<state_dir>/policy.cbor` so that
trusted client/selector pairs don't re-prompt on every reconnect. View them
with `tau policy-show`.


## Built-in extensions

Most built-in integrations are regular extensions under `crates/tau-ext-*/`.
They are configured under `extensions.<name>` in `harness.yaml` and can be
disabled with `enable: false`, swapped via `command:` / `prefix:`, or given
free-form `config:` payload that arrives at startup as a `LifecycleConfigure`
message. Some core tools, such as `delegate`, `wait`, and `skill`, are
harness-owned instead of extension processes.

```json5
extensions: {
  "core-shell":         { enable: false },                       // disable
  "provider-openai":    { prefix: ["ssh", "user@host"] },        // run remotely
  "std-notifications": { config: { idle_seconds: 30 } },         // reconfigure
},
```

### `core-shell` — shell and filesystem tools

Registers the everyday tools the agent uses to inspect and edit a project:
`shell`, `read`, `write`, `edit`, `grep`, `find`, `ls`, plus an `echo` tool
for testing. The shell command and any wrapper prefix are configurable:

```json5
"core-shell": {
  config: {
    shell: {
      command: "bash",
      prefix: ["nix", "develop", "-c"],
      // User-initiated `!`/`!!` commands are killed after this many
      // seconds. Tool-invoked `shell` calls use their own per-call
      // `timeout` argument (default 120s). Default: 3600 (1 hour).
      user_command_timeout_secs: 3600,
      // Extra env vars injected into `shell` and `!`/`!!` children,
      // applied after the inherited environment so they override or
      // supplement it. Use this to set a custom `PAGER` or adjust paths.
      extra_env: {
        XDG_CONFIG_HOME: "/home/me/.config",
        PAGER: "cat",
      },
    },
  },
},
```

Tau also discovers project and user agent context from conventional paths. It
loads `AGENTS.md` from `$HOME/.agents/`, from each current-working-directory
ancestor, and from matching `.agents.local/AGENTS.md` directories. Skills are
loaded from `.agents/skills` and `.agents.local/skills` under the current
working directory, plus `$HOME/.agents*/skills` and
`$HOME/.config/agents*/skills`. The `.local` variants are intended for
machine- or user-specific instructions and skills that should usually be added
to `.gitignore` instead of checked in.

Prompt fragments are composable too: top-level `harness.yaml`
`promptFragments` apply to every role, while `roles.<name>.promptFragments`
apply only to that role. Fragments are ordered by priority with extension- and
tool-provided fragments, so global style instructions, role guidance, and
tool-specific instructions share one prompt assembly path.

### `provider-openai` — OpenAI Responses backend

Publishes hardcoded `chatgpt/*` model metadata from provider-owned ChatGPT OAuth
state and owns model execution for that namespace. The harness assembles prompts,
then routes the selected provider's `session.prompt_created` event directly to
this extension; there is no built-in `core-agent` process.

Responses conversations chain via `previous_response_id` after the first turn:
each follow-up request sends only the messages added since the prior
`response.id` and lets the upstream API carry reasoning state forward
server-side. The chain is dropped automatically when the selected role resolves
to a different model, on branch edits
(`UiNavigateTree`), and turns that didn't return a `response_id`; if the
upstream rejects the stored id (server-side expiry), the provider falls back to
a full-replay retry once before surfacing the error.

The ChatGPT/Codex surface additionally routes turns over a persistent
**WebSocket** connection. One connection per `(account, session)` lives in a
small LRU pool inside `provider-openai`, so the server-side connection-local
response cache stays warm across turns of the same conversation — including
when Tau context-switches between sessions (sub-agent delegations interleaved
with the parent). Connections age out before the upstream's 60-minute hard cap,
and refreshed OAuth tokens invalidate stale sockets on next use.

### `std-notifications` — idle and turn notifications

Plays a sound on prompt submit and on the final response of a turn. After
`idle_seconds` of inactivity following a final response (default 60s) it emits
a desktop notification with a static "Waiting for user input" body — useful
when a long task finishes while you're in another window. Set
`config.idle_agent_summary: true` to restore the old behavior that asks the
agent for a one-sentence idle summary before notifying.

### Harness-owned `delegate` / `wait` — sub-task delegation

The harness exposes a `delegate` tool that spawns a side conversation and returns
its result to the caller, plus a `wait` tool for collecting background tool
results. Unless the `delegate` call supplies `role`, delegated sub-agents default
to the `engineer` role. The delegate placeholder and final result include
`self_agent_id` and `sub_agent_id`; pass `sub_agent_id` to the `message` tool
for live agent-to-agent notes. `message` can also target the special recipient `user`;
all messages are rendered in the UI as `Message from <sender> to <recipient>:`.
When `role` is supplied, or when the default `engineer` role is used, the
sub-agent runs with that role's resolved model, model parameters, system prompt,
and tool profile/filtering. The sub-agent starts with a *fresh* context — only
the parent's `prompt`, the selected role's system prompt, and the selected
role's tools — with no visibility into the parent conversation's prior turns,
tool results, or in-flight state. The same isolation applies at every nesting
depth, so sub-sub-agents don't see ancestor task framing and can't be tricked
into re-delegating it. Parent agents are responsible for putting everything the
sub-agent needs into the `prompt`. Live progress (turns, current tool) is shown
in the parent UI alongside the delegate's task name and role. See
[`docs/agent-messaging.md`](docs/agent-messaging.md) for messaging examples.

### Web search extensions

`std-websearch` proxies web search/fetch tools from one built-in extension. The
Exa-backed `websearch_exa` tool is enabled by default and advertised to models as
`web_search`. Parallel.ai tools are registered in the same extension with
internal names `websearch_parallel_search` / `websearch_parallel_fetch`,
advertised as `web_search` / `web_fetch`, but disabled by default so roles can
opt into them without duplicating the default `web_search` tool. Parallel uses
the default unauthenticated `https://search.parallel.ai/mcp` endpoint; Tau does
not support or send a Parallel API key. `config.parallel_endpoint` can override
the Parallel endpoint.


## CLI / UI

Tau ships a terminal UI that aims for *every pixel of estate is content* —
fast startup, no chrome. The prompt's right side shows the current working
directory, with `$HOME` shortened to `~`.

### Slash commands

Type `/` for menu autocompletion. The built-in set:

| Command             | Effect                                               |
| ------------------- | ---------------------------------------------------- |
| `/quit`             | Exit the session                                     |
| `/detach`           | Leave the UI, keep the harness running for reattach  |
| `/new`              | Start a fresh session in this harness                |
| `/model <role>`     | Switch agent role                                    |
| `/role <role> ...`  | Switch, create, edit, or delete an agent role        |
| `/fast`             | Toggle Codex Fast mode (`service_tier: fast`)        |
| `/tree [id]`        | Print session tree; with `id`, rewind head           |
| `/set <name> <val>` | Set a UI setting (Tab cycles names + values)         |

Available `/set` names include `show-diff` (expanded vs. compact diffs),
`show-thinking` (agent reasoning summaries), `show-cache-stats`
(prompt-cache hit stats in status bar), and `show-turn-stats` (per-turn
token usage below responses). These take `true` / `false`.
`/set show-messages <none|self-summary|self-full|all-summary|all-full>`
controls how agent/user messages are shown in the transcript. The first-arg
completion menu shows each setting's current value; the second-arg
menu shows the meaning of each allowed value. State is persisted to
`<state_dir>/cli.json`.

### Prompt input history

Submitted prompt lines are kept in prompt history for the current run and are
also appended to `<state_dir>/prompt-history.cbor`. New `tau` processes seed
Up/Down prompt recall from that file, so recent prompts from previous runs are
available like in-session history.

### Path autocompletion

When the prompt buffer starts with `./` or `../`, Tab triggers filesystem path
completion against the current working directory — handy for naming files in
free-form prompts. Standard fzf-style fuzzy-search bindings are also available
inside the completion menu. Slash-command arguments use the same menu but are
populated dynamically by the harness (model list, effort levels, …).

### Bang shell commands

A prompt line starting with `!` runs a shell command from the UI. `!<cmd>`
renders live stdout/stderr in the transcript and injects the finished output
back into the session context as a `<user_shell>` block, so the agent can see
what you ran and use the result.

Use `!!<cmd>` for UI-only commands: output is rendered the same way, but is
marked `[no context]` and is not replayed to the agent.

Examples:

```text
!ls
!!git status
```

### Customizable key bindings

`cli.yaml` exposes a `bind:` table that maps key chords to prompt-local
actions. Bindings are layered on top of built-ins; user entries with the
same key replace the built-in binding.

Supported actions:

- `submit-prompt`: submit the prompt, or accept a previewed completion without
  submitting.
- `insert-newline`: insert a newline at the cursor.
- `shell-prompt-edit`: dump the prompt to `$TAU_PROMPT_PATH`, run the shell
  command, then replace the prompt with the file contents on success.
- `shell-prompt-insert`: run the shell command and insert its stdout at the
  cursor on success.
- `fast-toggle`: toggle Fast mode directly. For example:
  `{ action: "fast-toggle" }`.
- `role-cycle`: cycle to the next available agent role directly. For example:
  `{ action: "role-cycle" }`.
- `prompt-history-search`: feed indexed prompt-history rows to a picker command,
  expose original prompts under `$TAU_PROMPT_HISTORY_DIR/<index>` for previews,
  then replace the prompt with the selected original prompt. The draft active
  when the picker opens is saved for prompt undo.

Command environment:

- `TAU_PROMPT_PATH`: tempfile containing the current prompt.
- `TAU_PROMPT_ROW` / `TAU_PROMPT_COLUMN`: 1-indexed cursor position for
  editor commands that support `file:row:column` syntax. Multi-line row
  calculation is still limited.
- `TAU_PROMPT_HISTORY_DIR`: for `prompt-history-search`, a temporary directory
  containing original prompt text files named by row index.

Default bindings:

```json5
bind: {
  "C-Enter": { action: "submit-prompt" },
  "C-f": {
    action: "shell-prompt-insert",
    command: "rg --files --hidden --glob '!.git' | fzf --height=100%",
    trim: true,
  },
  Tab: { action: "role-cycle" },
  "C-r": {
    action: "prompt-history-search",
    command: "fzf --height=100% --delimiter='\\t' --with-nth=2 --no-hscroll --preview 'cat \"$TAU_PROMPT_HISTORY_DIR\"/{1}' --preview-window 'right,60%,wrap' | cut -f1",
    trim: true,
  },
  "C-t": {
    action: "shell-prompt-insert",
    command: "RG_PREFIX='rg --line-number --column --no-heading --color=always --smart-case'; fzf --height=100% --ansi --disabled --bind \"change:reload:$RG_PREFIX {q} || true\" --delimiter : --preview 'bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null || awk -v line={2} '\\''line - 4 <= NR && NR <= line + 4 { printf \"%6d  %s\\n\", NR, $0 }'\\'' -- {1}' --preview-window '+{2}/2' | cut -d: -f1",
    trim: true,
  },
  "C-y": {
    action: "shell-prompt-insert",
    command: "if command -v jj >/dev/null 2>&1 && jj root --ignore-working-copy >/dev/null 2>&1; then jj log -r '::@' --no-graph -T 'change_id.shortest(8) ++ \"\\t\" ++ description.first_line() ++ \"\\n\"' | awk 'BEGIN { OFS=\"\\t\" } { id=$0; sub(/\\t.*/, \"\", id); title=$0; sub(/^[^\\t]*\\t?/, \"\", title); if (title == \"\") title=\"(no description set)\"; if (length(title) < 81) short=title; else short=substr(title, 1, 77) \"...\"; print id, short }' | fzf --height=100% --delimiter='\\t' --with-nth=2 --preview 'jj show --color=always {1}' --preview-window 'right,50%,wrap' | cut -f1; elif command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then git log --format='%h%x09%s' | awk 'BEGIN { OFS=\"\\t\" } { id=$0; sub(/\\t.*/, \"\", id); title=$0; sub(/^[^\\t]*\\t?/, \"\", title); if (title == \"\") title=\"(no description set)\"; if (length(title) < 81) short=title; else short=substr(title, 1, 77) \"...\"; print id, short }' | fzf --height=100% --delimiter='\\t' --with-nth=2 --preview 'git show --color=always {1}' --preview-window 'right,50%,wrap' | cut -f1; fi",
    trim: true,
  },
  "C-o": {
    action: "shell-prompt-edit",
    command: "${VISUAL:-${EDITOR:-}} \"$TAU_PROMPT_PATH\"",
  },
  "C-g": {
    action: "shell-prompt-edit",
    command: "${VISUAL:-${EDITOR:-}} \"$TAU_PROMPT_PATH\"",
  },
},
```

A Helix override:

```json5
bind: {
  "C-o": {
    action: "shell-prompt-edit",
    command: 'hx "$TAU_PROMPT_PATH:$TAU_PROMPT_ROW:$TAU_PROMPT_COLUMN"',
  },
  "C-g": {
    action: "shell-prompt-edit",
    command: 'hx "$TAU_PROMPT_PATH:$TAU_PROMPT_ROW:$TAU_PROMPT_COLUMN"',
  },
},
```

Use `trim: true` for commands like `fzf` whose selected value ends with a
newline you do not want inserted into the prompt.

### `Ctrl+O` — edit prompt in your editor

The default `C-o` binding suspends the UI, opens the prompt in `$EDITOR`, and
replaces the buffer with whatever you save. Redraws are paused while the
editor owns the terminal.

The editor file also includes a Markdown trailer after:

```md
<!-- TAU trailer: everything after this line will be ignored -->
```

Everything after the marker is ignored when Tau reads the file back. The
trailer quotes useful context for composing the next prompt: the current
in-flight response, the last agent response, and the previous submitted prompt.
Leading and trailing blank lines around the editable prompt are trimmed.

### `Ctrl+F` — fzf (or anything else) into the prompt

Because bindings are arbitrary shell commands, wiring fzf or another picker
into the prompt is straightforward:

```json5
bind: {
  "C-f": {
    action: "shell-prompt-insert",
    command: "rg --files --hidden --glob '!.git' | fzf --height=100%",
    trim: true,
  },
  "C-r": {
    action: "prompt-history-search",
    command: "fzf --height=100% --delimiter='\\t' --with-nth=2 --no-hscroll --preview 'cat \"$TAU_PROMPT_HISTORY_DIR\"/{1}' --preview-window 'right,60%,wrap' | cut -f1",
    trim: true,
  },
  "C-t": {
    action: "shell-prompt-insert",
    command: "RG_PREFIX='rg --line-number --column --no-heading --color=always --smart-case'; fzf --height=100% --ansi --disabled --bind \"change:reload:$RG_PREFIX {q} || true\" --delimiter : --preview 'bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null || awk -v line={2} '\\''line - 4 <= NR && NR <= line + 4 { printf \"%6d  %s\\n\", NR, $0 }'\\'' -- {1}' --preview-window '+{2}/2' | cut -d: -f1",
    trim: true,
  },
  "C-y": {
    action: "shell-prompt-insert",
    command: "if command -v jj >/dev/null 2>&1 && jj root --ignore-working-copy >/dev/null 2>&1; then jj log -r '::@' --no-graph -T 'change_id.shortest(8) ++ \"\\t\" ++ description.first_line() ++ \"\\n\"' | awk 'BEGIN { OFS=\"\\t\" } { id=$0; sub(/\\t.*/, \"\", id); title=$0; sub(/^[^\\t]*\\t?/, \"\", title); if (title == \"\") title=\"(no description set)\"; if (length(title) < 81) short=title; else short=substr(title, 1, 77) \"...\"; print id, short }' | fzf --height=100% --delimiter='\\t' --with-nth=2 --preview 'jj show --color=always {1}' --preview-window 'right,50%,wrap' | cut -f1; elif command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then git log --format='%h%x09%s' | awk 'BEGIN { OFS=\"\\t\" } { id=$0; sub(/\\t.*/, \"\", id); title=$0; sub(/^[^\\t]*\\t?/, \"\", title); if (title == \"\") title=\"(no description set)\"; if (length(title) < 81) short=title; else short=substr(title, 1, 77) \"...\"; print id, short }' | fzf --height=100% --delimiter='\\t' --with-nth=2 --preview 'git show --color=always {1}' --preview-window 'right,50%,wrap' | cut -f1; fi",
    trim: true,
  },
},
```

`C-r` searches prompt history (newest first), shows the selected prompt in an
fzf preview pane, and replaces the current draft with the selected original
prompt; `C-z` restores the draft that was active before the picker opened. `C-t`
starts with an empty result list; type a query
to search file contents with `rg`, preview the matching context, and insert the
selected file path. `C-y` opens a jj change picker when inside a jj repository,
falls back to git
commits in git repositories, and inserts the selected change or commit id.
Replace `rg --files | fzf`, the content-search command, or the commit picker
with `git ls-files`, a custom script, or whatever fits your workflow.

### Thinking / reasoning rendering

When the model emits reasoning blocks, the UI renders them inline above the
final answer, styled distinctly from the response. `/set show-thinking <true|false>`
toggles visibility globally; past blocks re-render in place when the flag flips, so
you can hide them after the fact. Reasoning blocks are not replayed back to
the provider as input — they remain provider-side context.

### Diff rendering

File mutations made by `write` and `edit` render as inline diffs. By default
they collapse to a compact `+N/-M` chip; `/set show-diff true` expands them to the
full unified hunk view. The terminal renderer uses cell-level differential
updates to avoid full repaints on each token.

### Theming

The UI ships with built-in dark and light "tau" themes. `cli.yaml` can set
`theme: dark`, `theme: light`, or `theme: auto`; `TAU_THEME=dark|light|auto`
overrides config for one process. Auto currently uses terminal background hints
such as `COLORFGBG` and falls back to dark. Themes map semantic style names
(`prompt.marker`, `prompt.cwd`, `banner.accent`, `system.info`, diff hunks,
reasoning blocks, …) to terminal attributes. See
`crates/tau-themes/themes/tau.json5` and `tau-light.json5` for the full style
key list.

### Session resume and detach

`/detach` leaves the harness daemon running so the agent can keep working in
the background; `tau --attach` reconnects later. `tau -r` opens a picker for
recent sessions in the current `cwd` (showing lock status and the latest user
prompt), `tau -r <id>` picks a specific one. The session tree, including
abandoned branches, is preserved across restarts.
