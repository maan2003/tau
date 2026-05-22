## Workspace layout

- `crates/tau` — main end-user binary that bundles first-party components
- `crates/tau-blocking-notify-channel` — blocking notification channel utility
- `crates/tau-cli` — CLI entrypoint: starts harness daemon and connects UI clients
- `crates/tau-cli-picker` — shared interactive picker for terminal selection prompts
- `crates/tau-cli-term` — higher-level terminal prompt: slash-command/path completion, menu rendering, `$EDITOR` integration
- `crates/tau-cli-term-raw` — raw terminal rendering/input layer
- `crates/tau-config` — user and project configuration loading
- `crates/tau-core` — event bus, routing, state, sessions, policy, and tool registry
- `crates/tau-ext-std-notifications` — built-in notification extension
- `crates/tau-ext-shell` — shell- and filesystem-oriented extension
- `crates/tau-ext-test-dummy` — test-only dummy extension
- `crates/tau-ext-websearch` — built-in generic web search extension (Exa default plus opt-in Parallel.ai tools)
- `crates/tau-extension` — extension-side protocol/runtime helpers
- `crates/tau-harness` — harness daemon: extensions, bus, sessions, socket server, harness-owned delegate/wait tools
- `crates/tau-provider` — provider credential/config library (storage, OAuth helpers, resolver)
- `crates/tau-provider-cli` — interactive `tau provider {add,remove,list,login,list-models}` subcommands
- `crates/tau-proto` — shared protocol types and CBOR codec helpers
- `crates/tau-skills` — skill discovery/loading support
- `crates/tau-socket` — Unix socket transport glue
- `crates/tau-supervisor` — supervised child-process and stdio transport glue
- `crates/tau-term-screen` — terminal screen layout and styled-cell renderer
- `crates/tau-test-support` — reusable end-to-end test utilities
- `crates/tau-themes` — themed text/style types

## Design docs

- `README.md` — project overview, install, configuration, and contact info
- `FEATURES.md` — major feature tour; update after editing any new major features
- `docs/` — focused design and feature notes
- `crates/*/README.md` — crate-specific documentation where present

## Common commands

- `cargo check --workspace --all-targets` or `just check` — check Rust code
- `cargo nextest run` or `just test` — run tests
- `treefmt` or `just format` — format code
- `selfci check` — full local CI verification; WARNING: slow, but independent of working copy state, so it can run safely in parallel, even while modifying other files; prefer to run in parallel and/or only as a final verification step

## Definition of done

- Code is formatted.
- Relevant tests pass.
- Run `selfci check` after every major change.
- Update `FEATURES.md` after editing any new major features.
