# Harness/Agent Item Architecture

This document describes the target architecture for Tau's harness/agent
conversation model.

For a Rust-shaped sketch of the same model, see
[`harness-agent-item-types.md`](harness-agent-item-types.md).

## Goals

- Keep the durable per-session event log as the source of truth.
- When building the next model request, preserve the order of assistant output
  and tool-result replies, rather than using the harness's
  execution/completion order.
- Keep operational routing and live UI state separate from transcript truth.
- Keep the model simple enough that replay, branching, and crash recovery stay
  understandable.

## Core Model

- The transcript is an ordered item timeline.
- `AgentResponseFinished` is assistant-output truth.
- `ToolRequest` is runtime machinery, not transcript truth.
- A single conversation may have at most one unresolved tool round at a time.
- A tool round may contain multiple tool calls.

Tau's semantic transcript nodes are:

- `UserInputNode`
- `AssistantResponseNode`
- `ToolResultsNode`
- `CompactionNode`

Tau's core transcript item kinds are:

- `Message`
- `ToolCall`
- `ToolResult`
- `Reasoning`
- `Compaction`
- `UnknownProviderItem`

Nodes describe transcript structure and lifecycle. Items are the ordered pieces
of conversation content that request assembly can flatten back into provider
input.

`AssistantResponseNode.output_items` preserves the model's original item order.
Tool calls live in that assistant response node because they are assistant
output.

`ToolResultsNode` is a grouped semantic node containing the terminal results
for one tool round, ordered by the original tool-call order.

## Durable Vs Runtime State

Durable per-session state stores protocol facts needed to reconstruct the
semantic transcript.

Runtime-only state handles:

- tool dispatch
- progress updates
- queued prompts
- per-conversation scheduling
- subscriber replay and acks

The harness keeps its live event-log delivery shape. That delivery plumbing is
not conversation state.

## Prompt Delivery To The Agent

The harness does not send the whole session log to the agent. It sends a
materialized prompt request for one turn.

That prompt request must:

- carry the full effective context in item form
- remain correct even if `previous_response_id` optimization is ignored
- support prompt compression with item-prefix refs rather than message-prefix
  refs

`previous_response_id` is therefore always a hint layered on top of a complete,
materializable prompt.

## Durable Facts

These remain durable transcript inputs:

- `UiPromptSubmitted`
- `SessionUserMessageInjected`
- `SessionPromptSteered`
- `AgentResponseFinished`
- terminal `ToolResult`
- `SessionCompacted`

These should be transient/non-durable:

- `ToolRequest`
- `SessionPromptQueued`
- `SessionPromptCreated`
- `SessionPromptPrewarmRequested`
- `SessionCompactionRequested`
- `AgentPromptSubmitted`
- progress-style lifecycle events

## Transcript Projection

The transcript tree is a deterministic projection over durable protocol facts.

Direct folds:

- `UiPromptSubmitted -> UserInputNode`
- `SessionUserMessageInjected -> UserInputNode`
- `SessionPromptSteered -> UserInputNode`
- `AgentResponseFinished -> AssistantResponseNode`
- `SessionCompacted -> CompactionNode`

Stateful folds:

- terminal `ToolResult` facts are buffered in fold state
- once every call in the current conversation's open round is terminal, the
  fold emits one `ToolResultsNode`

The fold may keep explicit pending-round state per conversation. That is the
right place to buffer non-folding tool-result facts.

If replay encounters a durable `ToolResult` that does not match any open tool
call in fold state, that session log is semantically invalid and replay should
fail for that session rather than silently skipping the event.

## Tool Round Semantics

Tool execution order is runtime policy. Transcript order is semantic.

Rules:

- persist `AgentResponseFinished` immediately once provider output is valid
- if it has no tool calls, the turn ends
- if it has tool calls, open one pending round for that conversation
- persist each terminal `ToolResult` as it arrives
- when the round becomes terminal, emit one `ToolResultsNode`
- only after that may the follow-up prompt be assembled

`ToolResultsNode` must be the direct child of the `AssistantResponseNode` whose
tool calls it completes. Because that parent relation is structural, no
separate `responds_to` field is needed on the node.

## Validation And Failure Semantics

Valid assistant output must be preserved even when Tau cannot or will not
execute the requested tool.

Examples:

- unavailable tool
- disabled tool
- locally rejected tool execution
- invalid Tau-visible tool name with otherwise valid model output

Those cases still commit the assistant response and produce terminal error tool
results.

Structurally malformed provider output is different. Missing/empty call ids,
duplicate call ids, or output that cannot be correlated into valid transcript
items must not commit as successful assistant responses. The transcript should
remain at the previous stable node.

## Tool Result Identity

- `call_id` must be globally unique for tool calls.
- Reusing a `call_id` is malformed provider output.
- The design may rely on globally unique `call_id`s when matching terminal
  results to open tool calls.

Tau already relies on this de facto in runtime routing; the redesign makes it
an explicit invariant.

## Queueing And Steering

Queued prompts stay runtime-only.

When a prompt arrives during an open tool round:

- the harness may decide at submission time that the prompt will be steered
- it is stored only in runtime queue state
- nothing durable is written yet

When the round completes successfully:

- the harness emits durable `SessionPromptSteered` event(s)
- each folds as a normal user-input node after the just-completed
  `ToolResultsNode`

When the round is cancelled:

- queued steering prompts are cancelled too
- no `SessionPromptSteered` event is emitted

Queued prompts are allowed to disappear on harness restart before they are
steered. That is acceptable.

`SessionPromptQueued` is therefore only operational information and should not
be durable transcript state.

## Cancellation And Resume

Cancellation must not leave a branch continuing through dangling tool calls.

Rules:

- if cancellation happens before a valid assistant response is committed, the
  transcript stays at the previous stable node
- if cancellation happens after assistant tool calls are committed, unresolved
  calls are completed with terminal cancelled results before the branch can
  continue

On resume after an interrupted tool round, unresolved tool calls should be
treated as cancelled rather than resumed silently.

## Compaction

Standalone compaction is the only transcript boundary.

- `SessionCompacted` folds to `CompactionNode { replacement_window }`
- request assembly stops walking older history at that boundary
- previous-response chaining does not cross that boundary

An embedded `ContextItem::Compaction` inside a normal assistant response is not
a boundary. It is preserved as opaque assistant output, and request assembly
continues walking past it.

## Previous-Response Chaining

`previous_response_id` is an optimization hint, not transcript truth.

Rules:

- prompts always carry the full materializable effective context
- the candidate may be ignored safely
- compaction boundaries break chaining
- transport/runtime liveness may invalidate the candidate
- tools, system instructions, and model parameters do not by themselves break
  the chain

The current implementation is more conservative here; that behavior is not the
target architecture.

## Lifecycle And UI State

Live lifecycle/progress state is operational, not transcript truth.

UIs and extensions should not infer turn completion from assistant-output
shape such as "tool_calls empty". The completed transcript comes from semantic
nodes; live rendering comes from operational lifecycle/progress events.

## Extensions And Side Conversations

Side conversations keep their own transcript branches.

- delegate/sub-agent tool calls are ordinary parent transcript tool calls
- the sub-agent conversation has its own transcript and tool rounds
- the final sub-agent outcome becomes the parent tool result
- non-tool extension side queries still enforce no-tools locally; if the model
  emits tool calls anyway, those calls become terminal error results rather
  than being silently dropped

## Conversation Identity

`ConversationId` remains runtime-local. It is not a durable protocol field.

The architecture should continue to rely on:

- tree parentage for folded nodes
- `session_prompt_id` for prompt/response ownership
- globally unique `call_id` for tool-call/result matching
- runtime-local conversation routing in the harness

## Deferred Problems

These are intentionally not solved by this document:

- cross-backend compatibility for opaque provider items such as reasoning items
  and other unknown provider artifacts
- improving current lossy behavior when replaying opaque items through backends
  that cannot represent them

For now, existing behavior may be preserved there and improved later.

## Summary Invariants

- Durable per-session protocol events are the source of truth.
- Transcript projection is deterministic.
- Assistant output order is preserved exactly.
- `ToolRequest` is not used to reconstruct transcript tool calls.
- One conversation has at most one unresolved tool round at a time.
- `ToolResultsNode` is emitted only when that round is terminal.
- `ToolResultsNode` is the direct child of the `AssistantResponseNode` it
  completes.
- `call_id` is globally unique.
- `ConversationId` is runtime-local only.
- Standalone compaction is the only context boundary.
