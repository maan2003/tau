# Agent messaging tool

The harness-owned `message` tool lets an agent send an asynchronous short text note to the user or to another agent. Every sent message is recorded as an `agent.message` event. UI display depends on `/set show-messages`; when shown fully it renders as:

```text
Message from <sender> to <recipient>:
<message>
```

`/set show-messages` modes are:

- `none`: no UI indication or history of any messages
- `self-summary`: one-line no-content indication for messages from or to the user; no UI indication for agent-agent messages
- `self-full`: full content for messages from or to the user only
- `all-summary`: full content for user messages plus one-line no-content indication for agent-agent messages
- `all-full`: full content of all messages

## Send to the user

Use the special recipient id `user`:

```text
message({"recipient_id":"user","message":"I found the root cause and am checking the fix now."})
```

On success the tool result is:

```text
Message sent
```

## Send to another agent

Start the other agent with `delegate`. The instant background placeholder includes `self_agent_id` and `sub_agent_id` headers. The final delegate result carries the same ids alongside the sub-agent `output`:

```text
tau_internal: true
self_agent_id: engineer_parent
sub_agent_id: engineer_ab12cd34

Tool call `call_123` is running in the background.
```

Use `sub_agent_id` as `recipient_id`:

```text
message({"recipient_id":"engineer_ab12cd34","message":"Please also inspect crates/tau-cli/src/event_renderer.rs."})
```

The UI may display the message, summarize it, or hide it depending on `/set show-messages`. The recipient agent also receives a hidden internal prompt with the message body XML-escaped inside a `<message>` wrapper.

## Invalid recipients and arguments

A non-`user` recipient must be a live or pending `agent_id`. Otherwise the tool fails and no `agent.message` event is emitted.

If the id was never known, the tool reports an unknown recipient:

```text
message({"recipient_id":"engineer_missing","message":"hello"})
```

```text
unknown message recipient: `engineer_missing`
```

If the id belonged to an agent that has already finished or was canceled before it could start, the tool reports a stopped recipient:

```text
message({"recipient_id":"engineer_done","message":"hello"})
```

```text
stopped message recipient: `engineer_done`
```

Tool arguments are schema-validated before dispatch. Unknown extra fields are rejected before any logical tool invocation is logged.
