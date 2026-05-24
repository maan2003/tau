# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

A role can set:

- `description`: short free-form summary shown in `/role ...` completions
- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, or `xhigh`
- `verbosity`: `low`, `medium`, or `high`
- `thinkingSummary`: `off`, `auto`, `concise`, or `detailed`
- `serviceTier`: `fast` or `flex`
- `compactionThreshold`: context-window percentage (0-100) at which automatic compaction starts for the role
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

Roles live in `harness.yaml` under globally unique `roleGroups`. Each group has a `roles` map, plus optional role fields such as `promptFragments` that apply as defaults to every role in the group. `defaultRole` selects the startup role; if omitted, Tau starts on the first role in `roleGroups` order.

```json5
{
  defaultRole: "senior-engineer",
  roleGroups: {
    engineer: {
      promptFragments: [
        { name: "engineer.workflow", priority: 66, text: "Focus on implementation details." },
      ],
      roles: {
        "junior-engineer": {
          description: "Lower-reasoning engineer",
          effort: "low",
        },
        "senior-engineer": {
          description: "Balanced coding engineer",
          model: "chatgpt/gpt-5.3-codex",
          effort: "medium",
          compactionThreshold: 85,
          tools: ["read", "grep"],
        },
        "staff-engineer": {
          description: "Maximum-reasoning engineer",
          effort: "xhigh",
        },
        "old-role": {
          enable: false,
        },
      },
    },
    manager: {
      roles: {
        manager: {
          promptFragments: [
            { name: "manager.workflow", priority: 66, text: "Delegate non-trivial work." },
          ],
        },
      },
    },
  },
}
```

Missing fields use group defaults first, then provider-published fallback knobs for the role's resolved model. When `compactionThreshold` is omitted, Tau uses its built-in automatic compaction threshold. Set `enable: false` on a role in a higher-precedence config layer to remove it from the effective role list and role-group cycling after all layers merge.

Tau ships built-in `junior-engineer`, `senior-engineer`, `staff-engineer`, and `manager` roles, with `defaultRole: senior-engineer`. `junior-engineer` uses lower reasoning for straightforward engineering work, `senior-engineer` uses balanced individual-contributor defaults, and `staff-engineer` is the maximum-reasoning engineering variant. `manager` is an orchestration role with a built-in delegation prompt. For non-trivial work, the built-in `manager` prompt tells the model to use `delegate` by default for research/scoping, implementation, and review/validation sub-agent steps, then synthesize the results; tiny or purely clerical work may still be handled directly.


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
