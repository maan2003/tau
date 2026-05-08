# Event interceptors

Event interceptors let a component handle an event emission before the event is appended to the harness event log and before normal subscribers see it.

This is a pre-log emission pipeline. While an event is intercepted, it is considered not emitted yet.

## Concepts

There are two conceptual layers:

- **Events** are facts that reach the event log and normal subscribers.
- **Harness messages** are protocol messages sent to or from the harness to control routing, delivery, and lifecycle behavior.

`harness.emit` is a message sent by a component to the harness asking it to emit an event. It is not itself the fact being emitted.

`harness.intercepted` is a directed harness message sent to an interceptor. It carries an event emission that has not reached the event log yet.

## Registering an interceptor

A component registers interception interest with `lifecycle.intercept`.

The message contains:

- `selectors`: event selectors to intercept
  - exact event names
  - prefixes
- `priority`: interception priority

Lower numeric priority values run first.

Registrations are owned by the connection that sent them. When that connection disconnects or crashes, the harness removes its interceptor registrations. If the component reconnects, it must register again during handshake.

## Matching

When the harness is about to emit an event, it checks the interceptor registry.

Exact selectors are preferred over prefix selectors. This preference is stronger than priority. For example, an exact interceptor at priority `100` runs before a prefix interceptor at priority `-100` for the same event.

Within the selected exact or prefix group, handlers are ordered by:

1. priority, ascending; lower values first
2. component name, ascending lexicographic order
3. connection id, ascending, as a final deterministic fallback


## Intercepted delivery

If a matching interceptor exists, the harness does not append the event to the event log and does not broadcast it to subscribers.

Instead, the harness sends the selected interceptor a directed `harness.intercepted` message:

```text
harness.intercepted {
  event,
  transient,
  interception
}
```

Fields:

- `event`: the not-yet-emitted event
- `transient`: the event-log persistence flag that would have applied to the event
- `interception`: the current interception priority

This delivery bypasses normal subscriptions. A component receives intercepted events because it registered as an interceptor, not because it subscribed to `harness.intercepted`.

## Interceptor outcomes

An interceptor has three normal choices.

### Drop

The interceptor can do nothing.

The event is consumed and never reaches later interceptors, the event log, or normal subscribers.

This is expected behavior, not an error.

### Pass unchanged

The interceptor can send `harness.emit` back to the harness with the same event and same metadata.

That resumes the interception chain after the current interceptor.

### Pass modified

The interceptor can send `harness.emit` back with a modified event and/or modified metadata.

Later interceptors and final subscribers see the modified event.

## Redelivery cursor

`harness.emit` includes an optional `interception` field.

```text
harness.emit {
  event,
  transient,
  interception
}
```

The field controls where interception scanning starts.

- `interception: null` starts scanning from the beginning.
- `interception: P` resumes after the sender at priority `P`.

The component id is not included in the payload. The harness knows which connection sent the redelivery message and uses that as the component part of the cursor.

This prevents a component from pretending to be a different interceptor in the same priority level. It can still intentionally restart scanning by setting `interception` to `null`, or jump to another priority by setting a different priority.

There is no loop guard. A component can create an interception loop by repeatedly redelivering from the beginning. This is intentional for now; redelivery is asynchronous and can happen arbitrarily later, so robust loop tracking would require extra event identity and lifecycle machinery.

## Same-priority chaining

Suppose the ordered interceptors for an event are:

```text
(priority 10, component alpha)
(priority 10, component beta)
(priority 20, component gamma)
```

Initial emission starts with no cursor, so `alpha` receives the event.

If `alpha` redelivers with `interception: 10`, the harness resumes after `(10, alpha)`, so `beta` receives the event.

If `beta` redelivers with `interception: 10`, the harness resumes after `(10, beta)`, so `gamma` receives the event.

If `alpha` redelivers with `interception: null`, scanning restarts from the beginning and `alpha` receives the event again.

## Final emission

If scanning finds no remaining matching interceptor, the harness finally emits the event normally:

1. apply session persistence rules, unless `transient` is set
2. append to the harness event log
3. publish the event-log wrapped event to normal subscribers

Only this final step makes the event visible as an emitted fact.

## Transience

The `transient` flag is carried through interception.

An interceptor should preserve it when passing the event along unless it intentionally wants to change whether the final event is durable.

Events that default to transient still get that default when initially emitted through the normal harness path. While intercepted, that transient value is included in `harness.intercepted` and should be sent back in `harness.emit` on redelivery.

## Debugging

The harness logs interception decisions with tracing under the `tau_harness::interception` target.

These logs are diagnostic only. They are not event-log entries and are not visible to normal event subscribers.

## Example flow

A component registers:

```text
lifecycle.intercept {
  selectors: [Exact("ui.prompt_draft")],
  priority: 0
}
```

Another component emits:

```text
harness.emit {
  event: ui.prompt_draft { ... },
  transient: true,
  interception: null
}
```

The harness finds the interceptor and sends it:

```text
harness.intercepted {
  event: ui.prompt_draft { ... },
  transient: true,
  interception: 0
}
```

The interceptor modifies the event and passes it on:

```text
harness.emit {
  event: ui.prompt_draft { modified ... },
  transient: true,
  interception: 0
}
```

If no later interceptor matches, the modified `ui.prompt_draft` is emitted normally.
