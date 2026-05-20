# tau-ext-core-subagents

Core tool extension that registers `delegate` and `wait`.

`delegate` takes a `prompt` string, starts a side conversation through the harness via `ExtAgentQuery`, and returns only the delegated agent's final text as the tool result.

`wait` takes a `tool_call_id`, waits for that tool call to finish, and returns the final background result/error once.
