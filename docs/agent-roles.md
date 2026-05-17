# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

A role can set:

- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, or `xhigh`
- `verbosity`: `low`, `medium`, or `high`
- `thinkingSummary`: `off`, `auto`, `concise`, or `detailed`
- `serviceTier`: `fast` or `flex`
- `toolsProfile`: name of a tool-availability profile from `harness.json5`

Roles live in `harness.json5` under `roles`:

```json5
{
  roles: {
    smart: {
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
  },
}
```

Tool profiles themselves live in `harness.json5` under `toolsProfiles`:

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

Tau ships built-in `smart`, `deep`, and `rush` roles. `smart` is the startup fallback role; `deep` asks for higher reasoning with detailed thinking summaries; `rush` asks for lower reasoning.


## Selecting a role

Use `/model <role>` or `/role <role>`.

`/model` and `/role` completion list roles, not raw models. Each completion description shows the currently resolved model and role settings.


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

The convenience commands `/effort`, `/verbosity`, `/thinking-summary`, and `/fast` mutate the currently selected role using the same role-update path.

The `<role>` argument completes existing roles, but any new name can be used to create a role for the current run. Add it to `roles` if it should be available after restart.

`/role <role> delete` removes the runtime/persisted role override. It does not edit `roles` from configuration; built-in or configured roles come back on the next harness start.

Runtime changes for built-in or configured roles are persisted in `~/.local/state/tau/harness.json5` together with the last selected role.
