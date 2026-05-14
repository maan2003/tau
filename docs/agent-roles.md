# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

A role can set:

- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, or `xhigh`
- `verbosity`: `low`, `medium`, or `high`
- `thinkingSummary`: `off`, `auto`, `concise`, or `detailed`
- `fastMode`: `true` or `false`
- `toolsProfile`: name of a tool-availability profile from `harness.json5`

Roles live in `models.json5` under `defaultRoles`:

```json5
{
  defaultRoles: {
    smart: {
      model: "openai/gpt-5.3-codex",
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
    },
  },
}
```

Tool profiles themselves live in `harness.json5` under `toolsProfiles`:

```json5
{
  toolsProfiles: {
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
tool's extension-provided `enabled_by_default` setting.

Missing fields use Tau's hardcoded defaults for the selected model.

Tau ships built-in `smart`, `deep`, and `rush` roles. `smart` is the startup fallback role; `deep` asks for higher reasoning with detailed thinking summaries; `rush` asks for lower reasoning and Fast mode.


## Selecting a role

Use `/model <role>`.

`/model` completion lists roles, not raw models. Each completion description shows the currently resolved model and role settings.


## Editing roles

Use:

```text
/role <role> <delete|model|effort|verbosity|thinking-summary|fast-mode|tools-profile> [value]
```

Examples:

```text
/role smart model openai/gpt-5.3-codex
/role deep effort xhigh
/role rush fast-mode on
/role smart tools-profile read_only
/role temporary model anthropic/claude-sonnet-4-20250514
/role temporary delete
```

The `<role>` argument completes existing roles, but any new name can be used to create a role for the current run. Add it to `defaultRoles` if it should be available after restart.

`/role <role> delete` removes the runtime/persisted role override. It does not edit `defaultRoles` from configuration; built-in or configured roles come back on the next harness start.

Runtime changes for built-in or configured roles are persisted in `~/.local/state/tau/harness.json5` together with the last selected role.
