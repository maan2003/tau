# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

A role can set:

- `description`: short free-form summary shown in `/role ...` completions
- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, or `xhigh`
- `verbosity`: `low`, `medium`, or `high`
- `thinkingSummary`: `off`, `auto`, `concise`, or `detailed`
- `serviceTier`: `fast` or `flex`
- `toolsProfile`: name of a tool-availability profile from `harness.yaml`
- `orchestrator`: when true, append a sorted list of available sub-task roles to this role's prompt

Roles live in `harness.yaml` under `roles`:

```json5
{
  roles: {
    smart: {
      description: "Balanced coding assistant",
      model: "chatgpt/gpt-5.3-codex",
      effort: "medium",
      toolsProfile: "full",
    },
    deep: {
      effort: "xhigh",
      thinkingSummary: "detailed",
    },
    rush: {
      effort: "low",
      thinkingSummary: "off",
      serviceTier: "fast",
    },
    foreman: {
      orchestrator: true,
    },
  },
}
```

Tool profiles themselves live in `harness.yaml` under `toolsProfiles`:

```json5
{
  toolsProfiles: {
    // Built in by default: prefer patch-style file mutation for GPT-family models.
    gpt: {
      apply_patch: true,
      edit: false,
      find: false,
      grep: false,
      ls: false,
      read: false,
      write: false,
    },
    full: {},
    read_only: {
      shell: false,
      write: false,
      edit: false,
    },
  },
}
```

When a role selects `toolsProfile`, each listed tool name overrides that
tool's extension-provided `enabled_by_default` setting. Tau includes a built-in
`gpt` profile that enables `apply_patch` and disables direct file/search tools
(`edit`, `write`, `read`, `grep`, `find`, and `ls`).

Missing fields use provider-published fallback knobs for the role's resolved model.

Tau ships built-in `smart`, `deep`, `rush`, and `foreman` roles. `smart` is the startup fallback role; `deep` asks for higher reasoning with detailed thinking summaries; `rush` asks for lower reasoning; `foreman` is an orchestration role with a built-in delegation prompt. For non-trivial work, the built-in `foreman` prompt tells the model to use `delegate` by default for research/scoping, implementation, and review/validation sub-agent steps, then synthesize the results; tiny or purely clerical work may still be handled directly.

When a role has `orchestrator: true`, Tau appends an `Available sub-task roles` section listing every role whose model is currently available so an orchestrator can pick an explicit role for delegated work. This list is appended even when the role's `prompt` is overridden.


## Selecting a role

Use `/model <role>` or `/role <role>`.

`/model` and `/role` completion list roles, not raw models. Each completion description shows the currently resolved model and role settings. `/role` completions also append the configured role `description` when present.


## Editing roles

Use:

```text
/role <role> <delete|model|effort|verbosity|thinking-summary|service-tier|tools-profile> [value]
```

Examples:

```text
/role smart model chatgpt/gpt-5.3-codex
/role deep effort xhigh
/role rush service-tier fast
/role smart tools-profile read_only
/role temporary model anthropic/claude-sonnet-4-20250514
/role temporary delete
```

Use `reset` as the value to clear a field and return to model/provider fallback behavior (`off` is still the explicit off value for `effort` and `thinking-summary`).

The convenience command `/fast` mutates the currently selected role using the same role-update path.

The `<role>` argument completes existing roles, but any new name can be used to create a role for the current run. Add it to `roles` if it should be available after restart.

`/role <role> delete` removes the runtime/persisted role override. It does not edit `roles` from configuration; built-in or configured roles come back on the next harness start.

Runtime changes for built-in or configured roles are persisted in the machine-readable `~/.local/state/tau/harness.json` together with the last selected role. Role `description`, prompt fragments, and `orchestrator` remain config-only metadata, so changing them in `harness.yaml` takes effect after restart without stale runtime state shadowing them.
