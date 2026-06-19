# tau-ext-std-notifications architecture

`std-notifications` is a user-facing side-effect bridge. It listens to harness events and emits terminal-facing notification actions; it does not own agent execution.

## Configuration keys

Hook and option keys use snake_case (`agent_start`, `agent_end`, `agent_idle`, `agent_idle_all`). Unknown fields must stay rejected so typoed notification config surfaces as a harness config error.

## Trigger boundaries

Prompt-start and turn-end notifications are user-visible main-turn effects. They use `PromptOriginator::User` prompt/provider events and ignore extension side conversations so delegate work and idle-summary queries do not ring sounds or perturb per-agent idle timers.

`agent_idle_all` uses harness-owned `agent.state` snapshots as its busy/idle source of truth. Provider final-response events only update template context (`turn.user_prompt` / `turn.agent_response`) for the eventual all-idle notification; they do not decide whether all agents are idle. This keeps side-query prompt/response traffic from clearing a pending all-idle notification.

## Idle timers

`agent_idle` timers are per completed user turn. `agent_idle_all` timers are armed when the tracked agent set transitions from at least one running agent to no running agents. A visible `agent.state = running` clears pending all-idle timers. Summary side agents spawned by this extension are correlated through `agent.start_accepted` for pending `idle-*` query ids and ignored for all-idle busy tracking until the matching `agent.start_result`, so they cannot cancel the notification they are producing. `ui.prompt_draft` extends idle timers that have not yet started summary side queries.
