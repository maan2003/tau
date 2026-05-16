# TODO: Item-Based Transcript Rework

## Problem

Tau currently lets operational tool events shape the model transcript.

A single model response can contain multiple tool calls:

```text
Assistant response:
  ToolCall A
  ToolCall B
```

But the persisted session tree can currently become ordered by execution/runtime events:

```text
ToolRequest A
ToolResult A
ToolRequest B
ToolResult B
```

That is wrong for provider replay. The model emitted both tool calls before seeing either result, so the next request should represent:

```text
ToolCall A
ToolCall B
ToolResult A
ToolResult B
```

The root issue is that Tau currently mixes two concepts:

1. **Operational event log** — what the harness/runtime did.
2. **Conversation transcript** — what the model should see on the next request.

`ToolRequest` is operational machinery. It must not be the source of truth for assistant tool-call items in the conversation transcript.

## Core Decisions

```text
AssistantResponse is conversation truth.
ToolRequest is runtime machinery.
ToolResults is the semantic reply to an assistant response's tool calls.
Provider adapters are item converters.
```

- The conversation transcript is an ordered item timeline.
- Request-building consumes only semantic transcript nodes/items.
- Operational events may be delivered live and captured in debug logs, but durable per-session persistence should be reserved for facts needed to reconstruct session/transcript state.
- The append-only per-session protocol event log remains the persistent source of truth.
- The transcript tree is a deterministic projection from persisted protocol facts, not separately persisted state.
- Chat Completions is an adapter over Tau's item transcript; Tau's core model should not be Chat-Completions-shaped.

## Source Of Truth And Projections

Tau should keep the existing good architectural shape: persist protocol events, then derive in-memory views by folding.

Today there are two log-like mechanisms with different jobs:

```text
in-memory harness EventLog
  -> live LogEvent delivery, subscriber replay, acks

durable per-session events.cbor
  -> PersistedSessionEvent stream
  -> SessionTree projection
```

The item rework should preserve that split. The durable per-session log should store protocol facts, not precomputed transcript nodes and not extra operational audit just for debugging. Non-persistent live/debug events can still exist for UI progress, timing, and inspection.

The transcript tree is a deterministic projection over durable protocol facts. Some events map directly to transcript nodes; some update fold state and only produce a node once enough related facts have arrived.

Example direct folds:

```text
UiPromptSubmitted          -> UserInput node
SessionUserMessageInjected -> UserInput node
SessionPromptSteered       -> UserInput node
AgentResponseFinished      -> AssistantResponse node
SessionCompacted           -> Compaction node
```

Example stateful/grouped fold:

```text
AgentResponseFinished [ToolCall A, ToolCall B]
  -> append AssistantResponse node
  -> open pending tool round for A, B

ToolResult B
  -> record terminal result for B; no transcript node yet

ToolResult A
  -> record terminal result for A
  -> all calls terminal, append ToolResults node ordered [A, B]
```

The durable event order may be `ToolResult B` then `ToolResult A`, but the derived transcript is ordered by the original assistant tool-call order. This is the key change: the fold/projection may normalize and group facts, instead of blindly appending one tree node per event in log order.

A persistent event that affects the transcript projection must contain enough data to replay deterministically, including explicit parent/branch identity or enough stable linkage to recover it. The tree is a materialized view of these events; it should never be the durable source of truth.

## Harness To Agent Delivery

The harness should not send the whole event log to the agent as a batch. It should keep the current event-log delivery shape:

```text
harness commits event
  -> append to in-memory EventLog
  -> if non-transient and session-attributed, append to durable per-session events.cbor
  -> broadcast LogEvent { id, recorded_at, event }
  -> subscribers ack LogEventId after processing
```

The bus matches subscriptions against the inner event name, not the `LogEvent` envelope. The agent subscribes to prompt-shaped events and receives individual committed events such as:

```rust
Message::LogEvent(LogEvent {
    id,
    recorded_at,
    event: Box::new(Event::SessionPromptCreated(prompt)),
})
```

The agent peels the envelope, processes the inner event, then sends a cumulative `Ack { up_to: id }`. Ack tracking is delivery/replay plumbing; it is not conversation state.

In the item model, `SessionPromptCreated` should carry the full materializable prompt request:

```rust
struct SessionPromptCreated {
    session_prompt_id: SessionPromptId,
    session_id: SessionId,
    instructions: String,
    context_items: Vec<ContextItem>,
    context_item_prefix: Option<PromptItemPrefix>,
    tools: Vec<ToolDefinition>,
    tools_ref: Option<PromptToolsRef>,
    model: Option<ModelId>,
    model_params: ModelParams,
    tool_choice: ToolChoice,
    originator: PromptOriginator,
    previous_response_candidate: Option<PreviousResponseCandidate>,
}
```

`context_items` must be correct if `previous_response_candidate` is ignored. The candidate is only an optimization hint for the agent/provider adapter.

## Core Data Model

### Context items

Use one provider-ish item enum for all transcript context, instead of separate `UserInputItem`, `AssistantOutputItem`, and `ToolResultItem` families.

Transcript nodes describe lifecycle/ownership. `ContextItem` describes what can be flattened into provider input.

```rust
enum ContextItem {
    Message {
        role: ContextRole,
        content: Vec<ContentPart>,
        phase: Option<MessagePhase>,
        provider_item_id: Option<String>,
    },

    ToolCall {
        call_id: ToolCallId,
        provider_item_id: Option<String>,
        name: ToolNameMaybe,
        tool_type: ToolType,
        arguments: CborValue,
    },

    ToolResult {
        call_id: ToolCallId,
        tool_type: ToolType,
        status: ToolResultStatus,
        output: CborValue,
    },

    Reasoning {
        raw_provider_item: CborValue,
        compatibility: ProviderItemCompatibility,
    },

    Compaction {
        raw_provider_item: CborValue,
        compatibility: ProviderItemCompatibility,
    },

    UnknownProviderItem {
        raw_provider_item: CborValue,
        compatibility: ProviderItemCompatibility,
    },
}

enum ContextRole {
    System,
    Developer,
    User,
    Assistant,
}

enum ToolResultStatus {
    Success,
    Error { message: String },
    Cancelled { reason: String },
}
```

Notes:

- `Message` covers user, assistant, developer, and system-shaped provider items. Tau may still send its current system prompt as request-level instructions rather than transcript context, but compaction output or future providers may produce system/developer-shaped retained items.
- `Reasoning`, `Compaction`, and `UnknownProviderItem` preserve opaque provider state that Tau should pass through without trying to understand.
- Provider-specific opaque items carry compatibility metadata. If the active backend/model cannot consume an opaque item, request assembly must fall back to a compatible compacted/textual window or fail loudly rather than silently corrupting context.

### Transcript nodes

Transcript nodes are the branch/lifecycle units produced by the deterministic projection over persisted protocol facts.

```rust
enum TranscriptNode {
    UserInput(UserInputNode),
    AssistantResponse(AssistantResponseNode),
    ToolResults(ToolResultsNode),
    Compaction(CompactionNode),
}

struct UserInputNode {
    items: Vec<ContextItem>,
}

struct AssistantResponseNode {
    response_node_id: ResponseNodeId,
    provider_response_id: Option<String>,
    backend: AgentBackend,
    output_items: Vec<ContextItem>,
    usage: Option<TokenUsage>,
}

struct ToolResultsNode {
    responds_to: ResponseNodeId,
    items: Vec<ContextItem>, // all ContextItem::ToolResult
}

struct CompactionNode {
    replacement_window: Vec<ContextItem>,
}
```

Rules:

- A user input node usually contains one `ContextItem::Message { role: User, ... }`, but the model allows richer user context later.
- One completed valid model response persists as one immutable `AssistantResponseNode`.
- The order of `AssistantResponseNode.output_items` is the model's output order. Runtime behavior must not reorder it.
- Assistant responses may contain text messages, tool calls, reasoning items, compaction items, or unknown provider items.
- Tool calls live in `AssistantResponseNode` because they are assistant output, not because Tau dispatched a tool.
- `ToolResultsNode.responds_to` links a tool-results node to the assistant response whose tool calls it answers. It is used for ordering, crash recovery, orphan-result detection, rendering, and branch safety.
- `ToolResultsNode.items` are ordered by the `ToolCall` order in the referenced response, not by runtime completion order. OpenAI correlates outputs by `call_id`, but provider input is still an ordered item list, so Tau should keep deterministic semantic order and avoid leaking runtime completion order into model context.
- `CompactionNode` is a context-window replacement boundary. There is no `human_summary` field in the core model.

## Request Building

Request-building must not consult `ToolRequest`.

It walks the transcript branch and converts semantic nodes/items into provider request items:

```text
UserInput           -> context item(s)
AssistantResponse   -> prior assistant output item(s)
ToolResults         -> tool result item(s)
Compaction          -> replacement context window and stop-before-boundary behavior
```

For the Responses API this maps naturally:

```text
ContextItem::Message { role: User, ... }
  -> { type: "message", role: "user", content: [...] }

ContextItem::Message { role: Assistant, ... }
  -> { type: "message", role: "assistant", content: [...] }

ContextItem::ToolCall
  -> { type: "function_call" | "custom_tool_call", call_id, name, arguments/input }

ContextItem::ToolResult
  -> { type: "function_call_output" | "custom_tool_call_output", call_id, output }

ContextItem::Reasoning
  -> raw provider reasoning item replay

ContextItem::Compaction
  -> raw provider compaction item replay

ContextItem::UnknownProviderItem
  -> raw provider item replay when compatible, otherwise fail/fallback explicitly
```

### Effective context and compaction

Request assembly derives an effective context window from the branch.

Semantic rules:

- A standalone/provider compaction operation commits `CompactionNode { replacement_window }`.
- The replacement window is the canonical context window to use after compaction.
- Request assembly stops walking older transcript history once it reaches a compaction boundary.
- A normal model response may emit `ContextItem::Compaction` in its ordered output items. Tau preserves it as an item and continues; for later request assembly, the latest compaction item acts as a boundary and older items before it are not replayed.
- Compaction destroys `previous_response_id` anchors at or before the boundary. Send the compacted context explicitly; later responses after that request may create fresh anchors.

Example:

```text
UserInput old
AssistantResponse old
Compaction [replacement_window]
UserInput new
```

Next request context:

```text
replacement_window
UserInput new
```

No older items are replayed. No previous-response anchor from before the compaction is used.

### `previous_response_id` candidate

`previous_response_id` is a provider optimization, not transcript truth.

The harness should derive a candidate while building `SessionPromptCreated`, but it should still send the full materializable effective context window. The prompt must be correct if the candidate is ignored.

```rust
struct PreviousResponseCandidate {
    response_node_id: ResponseNodeId,
    provider_response_id: String,
    next_item_index: usize,
    backend: AgentBackendRef,
    state_scope: PreviousResponseStateScope,
}

enum PreviousResponseStateScope {
    ProviderStored {
        transport: AgentBackendTransport,
    },
    RuntimeLocal {
        transport: AgentBackendTransport,
        session_id: SessionId,
        generation: String,
    },
}
```

Field meanings:

- `provider_response_id` is the id the provider may accept as `previous_response_id`.
- `next_item_index` is the slicing point in the full effective `context_items`: items before it are assumed covered by `provider_response_id`; items at/after it are the suffix to send if the candidate is used.
- `response_node_id` identifies the Tau transcript node the candidate anchors at. It is for validation, debugging, and diagnostics, not for the provider.
- `backend` ensures the candidate is only considered by a compatible provider adapter.
- `state_scope` lets the agent reject candidates whose provider state is no longer available.

Use rule:

```text
if candidate is usable for the current backend/runtime/transport:
  upstream.previous_response_id = candidate.provider_response_id
  upstream.input = context_items[candidate.next_item_index..]
else:
  upstream.previous_response_id = None
  upstream.input = context_items
```

Candidate derivation rules:

1. First derive the effective context window, applying compaction boundaries.
2. Find the latest ancestor `AssistantResponseNode` within that effective context that has a provider response id.
3. Create a candidate whose `next_item_index` points just after that response's contribution to the effective context window.
4. Do not create a candidate at or before a compaction boundary. A response containing a compaction item should conservatively be treated as a boundary, not as the anchor for the next request. Fresh anchors resume after a subsequent successful response over the compacted context.

Runtime validity rules:

- The candidate must be on the current branch and within the effective context window after compaction.
- The candidate must be from a backend/provider adapter that can be referenced by the current request.
- Tools, system instructions, and model parameters are request-scoped and should not by themselves invalidate the chain. They may still affect prompt-cache diagnostics, but they are not semantic proof that the previous response id is invalid.
- If the candidate is `RuntimeLocal`, the agent must verify the matching runtime state is still alive. For example, if a WebSocket pool entry closed and a new connection/generation was created, the old candidate is invalid and the agent must send full context.
- If the provider rejects a candidate as stale/incompatible, retry once with the full effective context window and no `previous_response_id`.

This keeps previous-response chaining as an agent/provider optimization. Harness correctness does not depend on the candidate being usable.

## Tool Round Lifecycle

Tool execution is separate from transcript order.

Harness lifecycle:

1. Receive ordered assistant output items from the agent.
2. Validate provider output structure.
3. Persist `AgentResponseFinished`; the transcript projection appends one immutable assistant response node.
4. Extract `ContextItem::ToolCall` items in response order.
5. If no tool calls exist, finish the turn.
6. If tool calls exist, create a pending tool round keyed by the assistant response node id.
7. Route transient operational `ToolRequest` events to providers according to scheduler/policy.
8. As providers finish, persist terminal `ToolResult` facts with `Success` / `Error` / `Cancelled` status and stable `responds_to` linkage.
9. Once the round is terminal, the transcript projection appends one `ToolResultsNode` ordered by the original response's tool-call order.
10. Only then assemble/send the follow-up prompt.

Scheduler rules remain runtime policy:

```text
Pure tools may run in parallel.
Mutating tools serialize per conversation.
Different conversations can proceed independently.
```

Important distinction:

```text
model output order != tool execution order != tool completion order
```

Only model output order belongs in `AssistantResponseNode.output_items`. Tool completion order may be preserved in operational events for UI/debugging, but not used as transcript order.

### Cancellation

Cancellation closes tool rounds with explicit cancelled tool results rather than abandoning committed assistant responses.

Rules:

- If cancellation happens before a valid assistant response is committed, leave the transcript head at the previous stable node and record cancellation operationally.
- If cancellation happens after an assistant response with tool calls is committed, every outstanding tool call gets a durable terminal `ToolResult { status: Cancelled { ... } }` fact.
- Completed tool calls keep their success/error result facts.
- Once all calls have terminal facts, the transcript projection appends one `ToolResultsNode` ordered by the original tool-call order.
- The branch may continue after that `ToolResultsNode`.

Invariant:

```text
No branch continues through dangling tool calls.
```

Example:

```text
AssistantResponse [ToolCall A, ToolCall B, ToolCall C]
ToolInvocationCompleted A
CancelRequested
ToolResults [Result A, Cancelled B, Cancelled C]
```

## Provider Output Validation

Do not silently synthesize IDs or rewrite provider output in the clean model.

### Valid model tool call, invalid Tau tool

Examples:

- unavailable tool,
- disabled tool,
- invalid Tau-visible tool name that still has a usable provider `call_id`,
- policy/interceptor denies execution.

These are valid assistant outputs. Commit the `ToolCall` item, do not execute if inappropriate, and answer it with an error `ToolResult` item.

### Structurally malformed provider output

Examples:

- missing/empty `call_id`,
- duplicate `call_id` in one response,
- tool call item cannot be correlated with future output,
- provider item shape cannot be converted into a valid `ContextItem`.

These are backend/provider failures. Do not commit them as a successful assistant response and do not execute tools. Emit an operational provider error, keep the transcript head at the previous stable node, and let retry/recovery happen from that stable transcript.

Streaming UI may show partial deltas operationally, but only a completed valid response becomes conversation truth.

## Operational Tool Events, Policy, And Interception

`ToolRequest` remains operationally important: it is how the harness asks an extension/tool provider to execute something.

Policy implications:

- If a tool is unavailable, disabled, rejected, invalid, or cancelled, the transcript still contains the original assistant `ToolCall` output item and receives a terminal error/cancelled result item.
- If an interceptor or policy layer denies execution, that becomes a tool result error, not deletion of the assistant tool call.
- If a future interceptor edits a tool invocation before execution, represent that override explicitly. Silently rewriting assistant output would corrupt the transcript; silently executing different arguments than the transcript shows would also be misleading. Prefer: original `ToolCall` stays immutable, operational execution records the override, and the result item makes the policy action visible.
- The pure-vs-mutating scheduler can serialize execution for safety, but transcript order remains response order.

## UI And Turn Status

UIs and extensions should not infer lifecycle state from `tool_calls.is_empty()` or equivalent bucketed fields.

The protocol should expose explicit lifecycle/status events or fields, for example:

```text
AgentTurnStarted
AgentResponseFinished
ToolRoundStarted
ToolInvocationStarted
ToolInvocationProgress
ToolInvocationCompleted
ToolRoundFinished
AgentTurnFinished
AgentTurnCancelled
AgentTurnFailed
```

The completed transcript view should come from semantic transcript nodes/items. Live progress should come from operational events.

`std-notifications`, CLI rendering, socket replay, and late subscribers need to move to this explicit lifecycle instead of treating `AgentResponseFinished` as both response content and turn-state signal.

## Agent And Provider Adapters

The agent should produce ordered output items directly, not bucketed fields.

Responses backend implications:

- Build a response item accumulator keyed by provider `output_index` / content indexes.
- Preserve separate assistant message items instead of flattening all text into one global string.
- Preserve reasoning, compaction, text, tool calls, and unknown provider items in output order.
- Continue emitting live operational updates for UI, but final conversation truth is ordered items.
- WebSocket and HTTP/SSE share the same item parser.
- Provider debug request/response dumps log item requests/responses.

Chat Completions implications:

- Collapse ordered Tau items into role messages only at the backend boundary.
- Convert returned assistant text/tool calls back into ordered `ContextItem`s.
- Accept that this adapter may be lossy for item types the Chat Completions API cannot represent.

## Extensions And Sub-Agents

Extension-facing tool contracts can mostly stay operational:

- Tool providers still receive `ToolRequest` and return `ToolResult` / `ToolError`-shaped terminal replies.
- The harness normalizes terminal replies into durable `ToolResult` facts with `Success` / `Error` / `Cancelled` status.
- The transcript projection groups those durable terminal facts into one `ToolResultsNode` per assistant response.
- `ExtAgentQuery` side conversations get their own transcript branches.
- Delegate/sub-agent tools are ordinary tool calls in the parent transcript; the sub-agent conversation has its own transcript and final result becomes the parent tool result.
- Non-tool extension side queries still need local no-tools enforcement. If the model emits tool calls anyway, commit the assistant output and answer those calls with terminal errors rather than silently dropping them.

## Implementation Impact

This is not only a prompt assembly refactor. It changes the contract between nearly every Tau layer that currently treats `ConversationMessage`, `AgentResponseFinished.text`, `AgentResponseFinished.tool_calls`, `ToolRequest`, or `ToolResult` as conversation state.

### Durable per-session event log changes

The durable log should remain a stream of protocol facts. We should not add a separate persisted semantic transcript log, and we should not persist operational/audit events solely for debugging.

Proposed durable event changes:

| Event | Change | Transcript projection |
| --- | --- | --- |
| `UiPromptSubmitted` | Keep. Payload may stay text-only initially; fold normalizes to `ContextItem::Message { role: User, ... }`. | Directly appends `UserInputNode`. |
| `SessionUserMessageInjected` | Keep. | Directly appends `UserInputNode`. |
| `SessionPromptSteered` | Keep. | Directly appends `UserInputNode` at the deterministic post-tool-results point. |
| `AgentResponseFinished` | Change payload from `text`/`tool_calls`/`reasoning_items` buckets to ordered `output_items: Vec<ContextItem>` plus backend/usage/provider response metadata. | Directly appends `AssistantResponseNode`, even for tool-only responses. Also opens a pending tool round if output contains tool calls. |
| `ToolResult` | Change from “one chronological tool-result node” to a terminal tool fact. Add stable linkage to the assistant response it answers, e.g. `responds_to: ResponseNodeId`, and fold success/error/cancelled status into one event shape. | Updates pending tool-round fold state; appends no node until every tool call from `responds_to` is terminal, then appends one ordered `ToolResultsNode`. |
| `ToolError` | Delete as a durable transcript fact, or keep only as an extension-facing/live alias that the harness normalizes into `ToolResult { status: Error }` before durable persistence. | No direct fold. |
| `SessionCompacted` | Change payload from summary/loose opaque strings to `replacement_window: Vec<ContextItem>`. | Directly appends `CompactionNode` and acts as a context boundary. |

Events that should become transient/non-durable for the per-session transcript log unless some separate recovery requirement appears:

- `ToolRequest` — still live/routing/UI, but the tool call itself is already in `AgentResponseFinished.output_items`.
- `SessionPromptCreated` — delivery to agent; reconstructable from transcript if needed.
- `SessionCompactionRequested` — delivery to agent; final durable fact is `SessionCompacted`.
- `SessionPromptPrewarmRequested` — optimization only.
- `AgentPromptSubmitted` — lifecycle/diagnostic only.
- `AgentResponseUpdated`, `ToolProgress`, `ToolDelegateProgress`, `ShellCommandProgress` — already progress-style transient events.

Events we should **not** add as durable facts:

- `UserInputCommitted` — use/adjust existing user protocol facts instead.
- `AssistantResponseCommitted` — use/adjust `AgentResponseFinished` unless we explicitly rename it.
- `ToolResultsCommitted` — `ToolResultsNode` is a projection artifact derived from terminal `ToolResult` facts.
- `CompactionCommitted` — use/adjust `SessionCompacted` unless we explicitly rename it.

### `tau-proto`

- Add first-class `ContextItem` / transcript node types.
- Add `PreviousResponseCandidate` as a prompt hint that accompanies the full materializable context.
- Replace `SessionPromptCreated.messages` and `SessionPromptPrewarmRequested.messages` with ordered request/context items.
- Replace `AgentResponseFinished.text`, `thinking`, `tool_calls`, and `reasoning_items` buckets with ordered assistant output items plus usage/backend metadata.
- Change `ToolResult` to represent success/error/cancelled terminal tool facts with stable `responds_to` linkage; remove or demote durable `ToolError`.
- Change `SessionCompacted` to carry a `replacement_window: Vec<ContextItem>`.
- Keep operational tool events (`ToolRequest`, `ToolProgress`, extension-facing `ToolError` if retained) for routing and UI, but keep them transient/non-folding.
- Update `GetSessionPromptCreated` / `SessionPromptCreatedResult` and prompt snapshot compression to reference item prefixes instead of message prefixes.
- Add explicit turn/tool-round lifecycle events or status fields.

### `tau-core`

- Keep `SessionTree` as a projection over `PersistedSessionEvent` records.
- Make the fold deterministic and item-aware, not one-node-per-event.
- `AgentResponseFinished` folds into one `AssistantResponseNode`, even when the response only contains tool calls.
- `ToolRequest` does not fold and should not be durable per-session state.
- `ToolResult` updates pending tool-round fold state; when all calls for `responds_to` are terminal, the fold appends one `ToolResultsNode` ordered by the original assistant response's `ToolCall` order.
- `SessionCompacted` folds as a context boundary/replacement window.
- Keep explicit parent node ids and conversation-local heads; do not reintroduce a global write-cursor dependency.

### `tau-harness`

- Prompt assembly consumes transcript items only and applies compaction boundaries.
- Tool scheduling routes transient operational `ToolRequest`s while separately collecting terminal `ToolResult` facts for durable persistence.
- Conversation state tracks pending tool rounds by response node id so live execution can stamp terminal tool facts with deterministic `responds_to` linkage.
- Prompt snapshots and compressed prompt refs become item-prefix based.
- Previous-response candidates are derived while building prompts, but harness sends full effective context so the agent can ignore invalid runtime-local candidates.
- Prewarm, compaction, context/token usage, side conversations, delegate progress, pending user prompt steering, cancellation, and duplicate-result handling all need item-aware updates.

### Debugging and replay

Debugging flows should remain able to answer:

- What did the model output, in exact order?
- What did Tau execute, in runtime order?
- What did each tool return, and when?
- What transcript was sent to the provider on the next request?

Preserve both views without making both durable transcript sources:

- durable protocol facts for request-building and completed conversation replay,
- transient/live/debug operational events for timing, progress, routing, and audit.

## Required Invariants

1. The durable per-session protocol event log is the source of truth.
2. The transcript tree is a deterministic projection from persisted protocol facts; it may group/reorder related facts according to transcript semantics.
3. `ToolRequest` / `ToolInvocationStarted` is never used to reconstruct assistant tool calls for provider replay.
4. `AssistantResponseNode` is immutable after commit.
5. If an assistant response contains tool calls and the branch continues, every tool call receives one terminal `ToolResult` status: `Success`, `Error`, or `Cancelled`.
6. `ToolResultsNode` is a projection artifact, not a required durable event; its item order is derived from the referenced response's tool-call order.
7. Operational completion order is recorded for UI/debugging only.
8. Conversation branch parentage is explicit; no global tree head determines where events fold.
9. Compaction is a context boundary; older context and older previous-response anchors do not cross it.
10. Provider adapters are mostly pure conversions between Tau context items and provider wire items.

## Migration / Implementation Sketch

Since we are willing to break compatibility, prefer a clean migration over compatibility shims.

High-level work:

1. Define `ContextItem`, transcript node types, item-prefix prompt refs, `PreviousResponseCandidate`, and explicit turn lifecycle events in `tau-proto`.
2. Replace `SessionPromptCreated.messages` with ordered context/request items.
3. Replace `AgentResponseFinished.text/tool_calls/reasoning_items` with ordered assistant output items and make it fold even for tool-only responses.
4. Change durable `ToolResult` into the terminal tool fact for success/error/cancelled, with stable `responds_to` linkage; remove or demote durable `ToolError`.
5. Make `ToolRequest`, `SessionPromptCreated`, `SessionCompactionRequested`, `SessionPromptPrewarmRequested`, and `AgentPromptSubmitted` transient/non-durable unless a specific recovery need appears.
6. Change `SessionCompacted` to carry `replacement_window: Vec<ContextItem>`.
7. Change `tau-core::SessionTree` to use deterministic item-aware projection, including pending tool-round fold state that emits `ToolResultsNode` only when a round is complete.
8. Change harness prompt assembly to consume transcript items only and apply compaction boundaries.
9. Derive `PreviousResponseCandidate` during request building from the effective context window while still sending full materializable context.
10. Teach agent/provider adapters to validate or ignore candidates based on backend runtime state, including WebSocket pool generation/liveness.
11. Change Responses backend to convert item-to-item with minimal reshaping.
12. Keep Chat Completions as a lossy compatibility adapter over ordered items.
13. Update CLI renderer, socket replay, and notifications to use explicit lifecycle/status events.
14. Add tests for parallel, mutating, synchronous, invalid, unavailable, cancelled, compaction, previous-response, and out-of-order-completion cases.

## Tests To Add

- Durable log contains `AgentResponseFinished [ToolCall A, ToolCall B]`, then `ToolResult B`, then `ToolResult A`; derived tree contains `AssistantResponse [A, B]` followed by `ToolResults [A, B]`.
- Single response emits two pure tool calls; result B completes first; next request order is call A, call B, result A, result B.
- Single response emits two mutating tool calls; execution serializes; next request still has call A, call B, result A, result B.
- First tool completes synchronously before second dispatch; next request still groups calls before results.
- Valid model tool call to unavailable/disabled tool; transcript preserves call and returns ordered error result.
- Structurally malformed provider tool call with missing/duplicate call id does not commit a successful assistant response or execute tools.
- Cancelled outstanding tool calls receive cancellation results before branch continues.
- Reasoning item interleaved with text/tool calls round-trips in original output order.
- Compaction node stops request assembly from walking older history.
- Streamed provider compaction item is preserved as an ordered context item and acts as a boundary for later request assembly.
- `PreviousResponseCandidate` is derived from the effective context window and does not cross compaction, while full context is still sent to the agent.
- Agent ignores a runtime-local previous-response candidate when its WebSocket/session generation is no longer live.
- Provider rejection of `previous_response_id` retries once with the full effective context window.
- `ToolRequest` events exist in live/operational delivery but are not durable per-session transcript facts and are ignored by request assembly.
