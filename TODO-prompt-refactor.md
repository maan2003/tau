# TODO: prompt/context refactor

This document describes the desired final architecture for Tau prompt rendering.
No intermediate compatibility or migration stability is required; implement as a hard cutover.

## Goals

- Move prompt-specific prose out of the harness core wherever possible.
- Let extensions publish structured session-scoped data that can be used by prompts and UI.
- Replace prompt "hooks" with named prompt fragments rendered as templates.
- Keep final system prompt assembly declarative in Handlebars templates.
- Avoid deep merge semantics entirely.
- Support multiple extensions collaborating on the same logical context key.
- Support one daemon serving multiple sessions in different directories with different context.

## Terminology

### Session context

Session context is structured data published for one session. It is not prompt-specific: the same data can power prompt templates, UI lists, slash-command completion, manual skill invocation, debug inspection, etc.

Use the name `SessionContextValue` as a plain JSON value alias/newtype, not an enum with merge modes.

```rust
pub struct SessionContextValue(pub serde_json::Value);
```

Each extension publishes the complete JSON value for its own `(session_id, key)` contribution. For collaborative keys, the value should usually be an array, but that is a convention of the key rather than a type-level mode.

Session context keys are always exposed to templates as a stable list of per-extension contributions:

```json
[
  { "extension_name": "core-skills", "value": [{ "name": "preview-site" }] },
  { "extension_name": "third-party", "value": [{ "name": "rust" }] }
]
```

Templates/fragments render this nested structure directly. The harness guarantees stable contributor ordering.

### Prompt fragment

A prompt fragment is a named Handlebars template contributed by the harness or an extension. It replaces the old prompt hook concept.

```rust
pub struct PromptFragment {
    pub name: PromptFragmentName,
    pub priority: PromptPriority,
    pub template: PromptContent,
}
```

After rendering:

```rust
pub struct RenderedPromptFragment {
    pub name: PromptFragmentName,
    pub priority: PromptPriority,
    pub content: PromptContent,
}
```

Use "fragment" terminology everywhere. Do not call these hooks.

## Session context publishing

Add an event/message for extensions to publish session context:

```rust
pub struct ExtSessionContextPublish {
    pub session_id: SessionId,
    pub key: SessionContextKey,
    pub value: SessionContextValue,
}
```

Storage is per session, per key, per contributor:

```text
session_id -> key -> contributor_connection_id -> SessionContextValue
```

A publisher replaces its own previous value for `(session_id, key)` by publishing again.

### Template exposure rules

No deep merging and no mode-specific merging. There is no singleton case.

For every session context key:

- Each contributor publishes one complete JSON value for that key.
- Publishing again replaces that contributor's previous value for `(session_id, key)`.
- The template-visible value is always a stable list of contribution wrappers:
  - `[{ extension_name, value }, ...]`
- `value` is exactly the contributor's latest JSON value.
- Deterministic contributor ordering should be used, e.g. by extension name / connection id.
- Templates/fragments render by contributor. The important invariant is stable ordering.

Do not support nested path merge semantics. Prefer top-level keys such as:

- `skills`
- `agents_files`
- `delegate_roles`
- `project`

## Session scoping and refresh

Session context is session-scoped because one daemon can have multiple sessions in different working directories/repositories.

Do not add a separate `HarnessSessionContextRefresh` event unless a later design needs explicit refresh requests. Extensions should normally react to existing session lifecycle/context events, especially new-session/session-init events that include the session id and working directory context.

Expected flow:

1. Harness starts or initializes a session and emits the existing session lifecycle event(s).
2. Extensions that care about repository/directory context inspect the session id and cwd/project context from those events.
3. Extensions publish their complete contribution for relevant keys:

```rust
ExtSessionContextPublish {
    session_id,
    key: "skills",
    value: SessionContextValue(serde_json::json!([...])),
}
```

If the effective cwd/project root changes, use the existing event that represents that change if one exists; otherwise add the narrowest lifecycle/context event for that state change. Extensions then replace their previous contribution for that session/key. Publishing an empty JSON array/object/null is the normal way for an extension to clear its own contribution, depending on that key's convention.

## Prompt render context

Prompt rendering derives a `PromptRenderContext` from:

- built-in harness facts, e.g. `cwd`, current role name, selected role metadata
- template-visible `session_context` contribution lists
- rendered prompt fragments

Example final Handlebars context shape:

```json
{
  "cwd": "/repo",
  "role": {
    "name": "foreman",
    "orchestrator": true
  },
  "session_context": {
    "skills": [
      {
        "extension_name": "core-skills",
        "value": [{ "name": "preview-site", "description": "..." }]
      }
    ],
    "agents_files": [
      {
        "extension_name": "core-shell",
        "value": [{ "path": "/repo/AGENTS.md", "content": "..." }]
      }
    ],
    "delegate_roles": [
      {
        "extension_name": "core-subagents",
        "value": [{ "name": "smart", "description": "...", "model": "openai/gpt-..." }]
      }
    ]
  },
  "prompt_fragments": [
    { "name": "skills.available", "priority": 100, "content": "..." },
    { "name": "delegate.available_roles", "priority": 200, "content": "..." }
  ]
}
```

`SessionContextValue` is not the same as `PromptRenderContext`: session context is the published per-extension JSON data; prompt render context is the final object passed to Handlebars. All extension-published session context must be nested under `session_context` so it cannot collide with built-in prompt fields such as `cwd`, `role`, or `prompt_fragments`. Each `session_context.<key>` is represented as a stable list of contribution wrappers.

## Prompt fragments

Replace existing `PromptHook` / `PromptHookPart` with prompt fragments.

Required fields:

- `name` — stable, preferably namespaced (`skills.available`, `delegate.available_roles`, `shell.instructions`)
- `priority` — controls order in final prompt
- `template` — Handlebars template content

Effective identity for replacement/debugging should be:

```text
(source_connection_id, fragment_name)
```

Ordering for final rendering:

```text
priority ASC, source_connection_id ASC, name ASC
```

Rendered fragments exposed to templates must include `priority` as data, not only use it for sorting. This lets the main system template or debug/inspection templates show or further group fragments by priority if desired.

Fragments are rendered with the same base prompt context that the main system prompt sees, except they should not need access to `prompt_fragments` themselves. Avoid recursion/weirdness.

If fragment rendering fails:

- log/diagnose with source and fragment name
- skip the failed fragment in the final prompt
- do not include raw unrendered template text in the model prompt

## Main system template

The top-level system prompt lives in a `.hbs` file, not Rust string literals.

It should be mostly stable harness framing plus generic fragment rendering:

```hbs
You are an expert coding assistant operating inside Tau, a coding agent harness...

Current working directory: {{cwd}}

{{#each prompt_fragments~}}
{{{content}}}

{{/each~}}
```

Use Handlebars whitespace control (`~`) where needed. Keep section prose in fragment templates rather than special-case Rust string composition.

`[tau-dedup]` should be hardcoded in the template, not passed as context.

## Handlebars helpers

Keep the `sort` helper.

Required behavior:

Session context data should be referenced under `session_context`, for example `session_context.skills` or `session_context.delegate_roles`.

- `{{#each (sort items)}}...{{/each}}` sorts scalar items themselves.
- `{{#each (sort items by="name")}}...{{/each}}` sorts objects by the given key.
- There is no implicit/default `name` key.

Contribution lists are exposed in stable contributor order. Templates can sort contributors or each contributor's `value` with the existing `sort` helper.

## Built-in context/fragments to move out of harness special cases

### Skills

Data:

```text
key = "skills"
value = SessionContextValue(json!([...]))
```

Each item should include at least:

```json
{ "name": "...", "description": "...", "add_to_prompt": true }
```

The skills prompt prose should be a prompt fragment template, not hardcoded in the main harness prompt builder.

The same `skills` session context should be usable by UI for manual skill invocation / listing.

### AGENTS.md

Data:

```text
key = "agents_files"
value = SessionContextValue(json!([...]))
```

Each item should include at least:

```json
{ "path": "...", "content": "..." }
```

Rendering AGENTS.md instructions should be done by a prompt fragment template.

### Delegate available roles

Extend role availability data so delegate can be driven from published/session context rather than harness-only prompt string generation.

Update `HarnessRoleInfo` to include resolved model availability:

```rust
pub struct HarnessRoleInfo {
    pub name: String,
    pub description: String,
    pub role_description: Option<String>,
    pub model: Option<ModelId>,
}
```

Then publish delegate role context, e.g.:

```text
key = "delegate_roles"
value = SessionContextValue(json!([...]))
```

Each item should include at least:

```json
{
  "name": "smart",
  "description": "Individual contributor...",
  "model": "openai/gpt-..."
}
```

The delegate extension should own the fragment template that renders available sub-task roles.

Remove `available_sub_task_roles_prompt` as a special pre-rendered string from the harness prompt builder.

### Tool instructions

Tool-specific instructions become prompt fragments. Existing tool registration should publish a `PromptFragment` rather than `PromptHookPart`.

## Hard cutover cleanup

Remove/rename old concepts:

- `PromptHook` -> prompt fragment collection type, if still needed
- `PromptHookPart` -> `PromptFragment`
- `tool_prompt_hook` -> `prompt_fragments`
- `gather_tool_prompt_hook_for_role` -> `gather_prompt_fragments_for_role`
- special Rust system prompt prose composition
- special Rust skill list rendering in system prompt
- special Rust AGENTS.md prompt string construction, if replaced by session context + fragment
- special `available_sub_task_roles_prompt`

No migration layer is required.
