# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

A role can set:

- `description`: short free-form summary shown in `/role ...` completions
- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, or `xhigh`
- `verbosity`: `low`, `medium`, or `high`
- `thinkingSummary`: `off`, `auto`, `concise`, or `detailed`
- `serviceTier`: `fast` or `flex`
- `promptFragments`: role-specific prompt fragments
- `promptOverride`: system prompt template name
- `tools`: explicit internal tools enabled for this role
- `disableTools`: internal tools disabled for this role

Top-level `promptFragments` in `harness.yaml` apply to every role. Use them for global style or policy instructions:

```yaml
promptFragments:
  - name: user.short-plain-style
    priority: 65
    text: Keep answers short and plain, using only simple words.
```

Roles live in `harness.yaml` under globally unique `roleGroups`. `defaultRole`
selects the startup role; if omitted, Tau starts on the first role in
`roleGroups` order.

```json5
{
  defaultRole: "engineer",
  roleGroups: {
    coding: {
      engineer: {
        description: "Balanced coding assistant",
        model: "chatgpt/gpt-5.3-codex",
        effort: "medium",
        tools: ["read", "grep"],
      },
      assistant: {
        effort: "off",
        serviceTier: "fast",
      },
    },
    planning: {
      manager: {
        promptFragments: [
          { name: "manager.workflow", priority: 66, text: "Delegate non-trivial work." },
        ],
      },
    },
  },
}
```

Missing fields use provider-published fallback knobs for the role's resolved model.

Tau ships built-in `assistant`, `engineer`, and `manager` roles, with `defaultRole: engineer`. `engineer` uses the same state-of-the-art individual-contributor defaults as the previous `smart` role. `assistant` is fast and lightweight with effort off. `manager` is an orchestration role with a built-in delegation prompt. For non-trivial work, the built-in `manager` prompt tells the model to use `delegate` by default for research/scoping, implementation, and review/validation sub-agent steps, then synthesize the results; tiny or purely clerical work may still be handled directly.


## Selecting a role

Use `/model <role>` or `/role <role>`.

`/model` and `/role` completion list roles, not raw models. Each completion description shows the currently resolved model and role settings. `/role` completions also append the configured role `description` when present.


## Editing roles

Use:

```text
/role <role> <delete|model|effort|verbosity|thinking-summary|service-tier|tools|disable-tools> [value]
```

Examples:

```text
/role engineer model chatgpt/gpt-5.3-codex
/role assistant service-tier fast
/role manager effort xhigh
/role engineer disable-tools shell
/role temporary model anthropic/claude-sonnet-4-20250514
/role temporary delete
```

Use `reset` as the value to clear a field and return to model/provider fallback behavior (`off` is still the explicit off value for `effort` and `thinking-summary`).

The convenience command `/fast` mutates the currently selected role using the same role-update path.

The `<role>` argument completes existing roles, but any new name can be used to create a role for the current run. Add it to `roleGroups` if it should be available after restart.

`/role <role> delete` removes the runtime role override. It does not edit `roleGroups` from configuration; built-in or configured roles come back on the next harness start.

Runtime role changes are not persisted. Startup is controlled by `defaultRole` and `roleGroups` order, and durable role changes should be made in `harness.yaml`.

Prompt fragment priorities sort ascending. Use priorities below `100` for role/persona instructions that should appear before generated context sections such as skills and AGENTS.md. Use high priorities for epilogue context; Tau's built-in current-working-directory fragment uses `900` so it stays at the end of the prompt.
