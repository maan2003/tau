# Message reference

Messages are control-plane traffic between a single component (extension,
agent, UI client) and the harness's per-component handler. They are
**point-to-point**: never broadcast on the bus, never written to the
durable session event log, and not subscribable.

Wire form: `{"message": "<flat_name>", "payload": {...}}` — flat
snake_case names, distinct from events' dotted `category.call` form.
The top-level [`Frame`](../crates/tau-proto/src/frame.rs) envelope tells
the two apart by discriminator. Type definitions live in
[`crates/tau-proto/src/messages.rs`](../crates/tau-proto/src/messages.rs).

For bus events (the broadcast `Event` half of the protocol), see
[events.md](events.md).

## Handshake

Exchanged when a component first connects to the harness. The order on
every connection is: client sends `hello`, then `subscribe` (and
optionally `intercept`); harness sends `configure` for supervised
extensions; client sends `ready` once setup is done.

- **`hello`** — A participant announces itself just after connecting:
  protocol version, client name, client kind (`agent` / `tool` / `ui`
  / `core` / `external`). First message on every connection.
- **`subscribe`** — A client declares which events the harness should
  deliver to it, as a list of selectors (exact name or prefix).
  Without a subscription, only directed traffic reaches the client.
- **`intercept`** — A client asks to receive matching event emissions
  *before* they hit the event log, with a priority. Lower priority
  runs first; the interceptor can rewrite the emission, drop it, or
  pass it through with a redelivery cursor so later interceptors at
  the same priority don't loop.
- **`ready`** — Sent by an extension after its own startup work is
  done and it is ready to participate in tool dispatch. The harness
  supervisor reacts by emitting the `extension.ready` *event* on the
  bus so subscribers can observe online state without watching every
  per-component pipe.
- **`disconnect`** — A client (or the harness) signals an intentional
  disconnect, with an optional human-readable reason. Distinct from a
  socket dying unannounced. The writer thread also sends this as a
  best-effort sentinel when shutting an extension's stdin.

## Configuration (harness → extension)

- **`configure`** — Sent point-to-point by the harness to one
  extension immediately after that extension's `hello`. Carries
  whatever the `config: { … }` value was for that extension in
  `harness.json5`, or an empty map when no config was provided.
  In-process extensions don't carry a supervised config and receive
  the empty default.
- **`config_error`** — An extension reports back that the `configure`
  payload it received was malformed or unusable; the harness surfaces
  the message just like a `harness.json5` parse error so the user can
  see why their per-extension config was rejected.

## Emission pipeline (client ↔ harness)

These wrap a real bus `Event` for delivery. They are messages — not
events — even though their payload is an event, because they're
point-to-point envelopes the bus never sees as facts.

- **`emit`** — A client's *request* to publish an event with
  harness-owned delivery metadata. Carries the inner event, a
  `transient` flag (don't persist to durable history), and an optional
  interception cursor (used when an interceptor is forwarding an
  emission it received earlier so the harness knows where to resume).
- **`intercepted`** — Directed harness → interceptor delivery of an
  emission that has not reached the event log yet, so the interceptor
  can act before subscribers see it. Carries the inner event, the
  same `transient` flag, and the priority cursor identifying which
  interceptor in the chain this is for.

## Transport (at-least-once delivery)

The harness's delivery layer wraps every published event in a
`log_event` envelope so receivers can ack after processing. Receivers
ack cumulatively — newer acks supersede older ones — and the harness
re-delivers from the last known position on reconnect.

- **`log_event`** — The harness's log-delivery envelope around a real
  bus event, carrying a monotonic `LogEventId`. Receivers peel the
  inner event, process it, then send an `ack` referencing the id (or
  any later id, since acks are cumulative).
- **`ack`** — Cumulative acknowledgement that the receiver has
  processed all log events with id `<= up_to`. Newer acks supersede
  older ones; duplicate or out-of-order acks are ignored by the
  harness.
