# Built-in HarnessSettings defaults. Harness-owned roles and tool profiles are
# provided as ordinary config so user partial overrides keep built-in fields.
# Contract and prompt-template metadata lives in built-in.harness.contracts.ncl;
# `tau-config` applies it around this file so the main defaults stay close to
# the plain Nickel records users write themselves. The remaining `default`
# annotations here are semantic merge priorities.
{
    session_retention_days | default = 60,
    extensions = {
        "provider-openai" = {
            suffix = ["ext", "ext-provider-openai"],
            role = "provider",
            enable | default = true,
            config = {},
        },
        "core-shell" = {
            suffix = ["ext", "ext-shell"],
            role = "tool",
            enable | default = true,
            config = {},
        },
        "test-dummy" = {
            suffix = ["ext", "ext-test-dummy"],
            role = "tool",
            enable | default = false,
            config = {},
        },
        "core-delegate" = {
            suffix = ["ext", "ext-core-delegate"],
            role = "tool",
            enable | default = true,
            config = {},
        },
        "std-notifications" = {
            suffix = ["ext", "ext-std-notifications"],
            role = "tool",
            enable | default = true,
            config = {
                idle_seconds | default = 60,
                idle_agent_summary | default = false,
            },
        },
        "std-websearch-exa" = {
            suffix = ["ext", "ext-websearch-exa"],
            role = "tool",
            enable | default = true,
            config = {},
        },
    },
    # Non-exported lazy role system-prompt templates are supplied by the
    # contract layer for built-in roles. Rust exports the settings with
    # eval_deep_for_export (which skips not_exported fields), then merges one
    # selected role prompt record with runtime context such as cwd, discovered
    # skills, and extension tool prompt hooks.
    roles = {
        smart = {
            description | default = "Individual contributor using state of the art model. Good default for most tasks.",
        },
        rush = {
            description | default = "Individual contributor using fast and cheaper model for smaller well-defined tasks.",
            effort | default = "low",
            thinkingSummary | default = "off",
        },
        deep = {
            description | default = "Deep reasoning expert, using potentially slower and more expensive model. Good for research and very complex tasks.",
            effort | default = "xhigh",
            thinkingSummary | default = "detailed",
        },
        foreman = {
            description | default = "Role focused on splitting and delegation of tasks to other sub-agents",
            systemPrompt = (
                let defaultSubTaskRoles =
                    roles
                    |> std.record.fields
                    |> std.array.filter (fun roleName => roleName != "foreman") in
                {
                    # The default foreman prompt derives its sub-task role list from
                    # the current role table, excluding foreman itself. Sibling role
                    # description overrides still flow into the lazy prompt without
                    # Rust passing role names at runtime. Keeping this list in a
                    # section lets user config tweak the selected roles or disable
                    # just one part of the built-in prompt.
                    sections = {
                        role.order | default = 50,
                        role.text | default = m%"
You are a foreman/orchestrator agent. Your job is to plan, coordinate, and synthesize work by delegating to sub-agents instead of doing all non-trivial work yourself.

Default workflow:

* For tiny, simple, or purely clerical tasks, you may work directly.
* For non-trivial tasks, use the `delegate` tool as the default path. Split the work into sub-agent steps for research/scoping, implementation, and review/validation. Split or repeat steps as needed for the task scope and difficulty.
* Delegate those steps instead of performing every detail yourself.
* Pass each sub-agent complete, self-contained instructions: the goal, relevant context, exact paths/symbols/snippets, constraints, tests/validation to run, and expected answer format.
* When useful, choose an explicit role for delegated work from the available sub-task roles list.
* Synthesize sub-agent results, resolve discrepancies, do only tiny or clerical follow-up directly, and report the final outcome.
"%,
                        availableSubTaskRoles.order | default = 60,
                        availableSubTaskRoles.roles | default = defaultSubTaskRoles,
                        availableSubTaskRoles.text | default =
                            if std.array.length availableSubTaskRoles.roles == 0 then
                                ""
                            else
                                "## Available sub-task roles\n\n%{harnessContracts.renderSubTaskRoleList roles availableSubTaskRoles.roles}",
                    },
                }),
        },
    },
    toolsProfiles = {
        gpt = {
            apply_patch | default = true,
            edit | default = false,
            find | default = false,
            gpt_shell | default = true,
            grep | default = false,
            ls | default = false,
            read | default = false,
            shell | default = false,
            write | default = false,
        },
    },
} | harnessContracts.BuiltInHarnessConfig
