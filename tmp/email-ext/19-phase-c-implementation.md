# Phase C implementation: extension-injected CLI action foundation

## Implemented

- Added `crates/tau-actions`, a dependency-light shared schema/parser crate.
  - Defines `ActionSchema`, `ActionCommand`, `ActionArg`, `ActionArgKind`, `ActionChoice`.
  - Validates schema version, root names (`/email` style), child/arg names, executable leaves, namespace/leaf separation, rest-string placement, enum choices, and duplicate action IDs.
  - Parses whitespace-token slash lines into action id, argv, and typed named args for nested commands and `string`/`integer`/`enum`/`rest_string` args.
- Added protocol support in `tau-proto`.
  - New action event names: `action.schema_published`, `action.invoke`, `action.result`, `action.error`.
  - New payloads: `ActionSchemaPublished`, `ActionInvoke`, `ActionResult`, `ActionError`, `ActionOutput`, `ActionInvocationId`.
  - Action events default to transient and round-trip through representative protocol tests.
- Added harness action registry/routing.
  - `tau-core::ActionRegistry` validates schemas, tracks owner-stamped providers, replaces per connection, unregisters on disconnect, and resolves action invocations.
  - Socket subscription policy now permits the `action` event family.
  - Harness stamps extension name + instance id before broadcasting `ActionSchemaPublished`.
  - Schemas received before `Ready` are staged and published when the extension activates.
  - Disconnect unregisters schemas and fails pending UI action invocations.
  - UI `ActionInvoke` is directed only to the owning extension.
  - Extension `ActionResult`/`ActionError` are source/action-id validated and directed only to the requesting UI client.
  - Late UI subscribers replay current action schemas from the registry.
- Added CLI integration.
  - CLI subscribes to `action.*`.
  - Renderer tracks dynamic action schemas and removes them on `ExtensionExited`.
  - Root dynamic slash commands are added to completion menus.
  - Submitted dynamic action lines parse client-side and dispatch `ActionInvoke` instead of `UiPromptSubmitted`.
  - Text action results/errors render as minimal system-info blocks; editor-buffer outputs render their title/text with an editable marker.
- Added extension helper.
  - `tau_extension::Handshake::publish_actions(schema)` emits an action schema during startup; harness stamps real owner fields.

## Intentionally deferred / follow-ups

- No real email `/email` approval actions were implemented in Phase C.
- Dynamic completions are root-level only. Nested subcommand/argument completion from `ActionSchema` remains a Phase C follow-up.
- If multiple live extensions publish the same dynamic root, the CLI deterministically keeps the first owner in sorted owner order; no user-facing conflict UI yet.
- `ActionOutput::EditorBuffer` is rendered as text only; opening/editing buffers is future UI work.
- No action cancellation protocol was added.

## Files changed

- Workspace/deps: `Cargo.toml`, `Cargo.lock`, touched crate `Cargo.toml`s.
- New: `crates/tau-actions/`.
- Protocol: `crates/tau-proto/src/{event_name.rs,events.rs,lib.rs,tests.rs}`.
- Core: `crates/tau-core/src/{action_registry.rs,lib.rs,policy.rs}`.
- Harness: `crates/tau-harness/src/harness.rs`, `crates/tau-harness/src/harness/replay.rs`, `crates/tau-harness/src/harness/tests/action.rs`.
- CLI: `crates/tau-cli/src/action_commands.rs`, `chat.rs`, `event_renderer.rs`, `lib.rs`; `crates/tau-cli-term/src/completion.rs`, `tests.rs`.
- Extension helper: `crates/tau-extension/src/handshake.rs`.

## Validation run

- `cargo fmt --check` ✅
- `cargo check -p tau` ✅
- `cargo check -p tau-actions -p tau-proto -p tau-core -p tau-cli -p tau-harness -p tau-extension` ✅
- `cargo test -p tau-actions -p tau-proto -p tau-core` ✅
- `cargo test -p tau-cli-term dynamic_slash_commands_are_in_root_completion_menu` ✅
- `cargo test -p tau-cli action_commands` ✅
- `cargo test -p tau-harness action::` ✅
- `cargo clippy -p tau-actions -p tau-proto -p tau-core -p tau-cli-term -p tau-cli -p tau-harness -p tau-extension -- -D warnings` ✅
- `cargo clippy -p tau-harness --all-targets -- -D warnings` ⚠️ failed on unrelated pre-existing `clippy::needless_borrow` in `crates/tau-harness/src/harness/tests/model.rs:993`.

Note: an earlier broader `cargo clippy ... --all-targets -- -D warnings` run also surfaced phase-owned warnings; those were fixed before the successful non-`--all-targets` clippy run above.

## Risks

- The CLI conflict policy for duplicate dynamic roots is intentionally simple and may need user-facing diagnostics before third-party extensions depend on it.
- The action parser is deliberately whitespace-token based and does not implement shell-style quoting; schemas should use `rest_string` for free-form trailing text.
