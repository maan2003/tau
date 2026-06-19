---
name: tau-self-knowledge-harness
description: >
  Use this skill when the user asks about the Tau harness daemon, including how
  it starts, accepts UI clients, uses Unix sockets, handles activation modes,
  socket activation, readiness signaling, attach behavior, or embedded harness
  runs.
advertise: false
---

# Tau harness daemon

The harness is Tau's central daemon. It owns agent runtime state, extension
supervision, event routing, agent prompt orchestration, tool routing, durable
agent logs, and per-run debug logs. UI clients connect to the harness;
extensions connect to the harness; the harness is the single coordinator between
them.

## Runtime-dir daemon mode

When the CLI starts a harness child, it creates a runtime-dir daemon unless it is
attaching to an existing daemon.

Common startup flow:

1. The CLI creates a child `tau component harness` process.
2. The harness creates a per-process socket path under Tau's runtime root and binds `<pid>.sock` inside `${XDG_RUNTIME_DIR}/tau/harnesses/` (or `/tmp/tau-$USER/harnesses/`).
3. The harness mints a short `HarnessRunId`, creates `<state_dir>/debug/<run_id>/`, enables `events.jsonl`, and publishes `harness.started { run_id }`.
4. After startup is ready for discovery, the harness writes `<pid>.json` metadata containing the project root and pid.
5. Later UI clients discover the daemon through that metadata and connect to the runtime-dir Unix socket.

The runtime-dir harness path always binds its generated socket path itself. It
does not use socket activation, because Tau attach/discovery expects the socket
to exist at the generated runtime-dir path.

## CLI-spawned initial UI stdio mode

When the terminal UI or one-shot CLI helpers start the harness, the CLI uses the
`tau component harness --initial-ui-stdio` mode.

In this mode:

- child stdin/stdout are reserved for the initial UI protocol connection,
- the CLI does not use those pipes for the readiness byte,
- the harness accepts stdin/stdout directly as the initial UI reader/writer,
- fatal startup failures are sent to the initial UI as protocol `Disconnect` frames when possible,
- extension startup and agent catch-up wait until that initial UI has connected and subscribed,
- runtime metadata is written after the startup state is ready for later socket attaches.

This prevents startup events from being missed by the UI that spawned the
daemon. Later UIs still attach over the normal runtime-dir Unix socket.

The older readiness-pipe handshake (`TAU_READY_FD`) has been removed.
CLI-spawned harnesses use initial UI stdio; attach mode connects to an existing
socket and does not spawn a child.

CLI-managed daemon spawns explicitly remove `LISTEN_FDS`, `LISTEN_PID`,
`LISTEN_FDS_FIRST_FD`, and `LISTEN_FDNAMES` from the child environment so
unrelated socket-activation wrappers cannot accidentally change normal Tau
startup.

## Attach mode

`tau --attach` does not start a new harness. It discovers an existing runtime-dir
daemon for the current project and opens a Unix socket connection to that
daemon.

Attach mode depends on runtime metadata being accurate. If no matching daemon
exists, attach fails instead of silently starting a new one.

## Foreground daemon APIs

The harness crate also exposes foreground daemon helpers such as `run_daemon`,
`run_daemon_with_config`, and test-only echo variants. These APIs take an
explicit socket path from the caller.

Foreground daemon APIs bind the provided path directly unless socket activation
provides a listener.

## Socket activation

Foreground daemon APIs support socket activation via the `listenfd` crate.

Behavior:

- the harness checks `ListenFd::from_env().take_unix_listener(0)`,
- if no listener is present, it binds the requested socket path normally,
- if a listener is present, it must be a Unix stream listener,
- the listener's local pathname must exactly match the requested socket path,
- Tau does not remove the socket path on shutdown when the listener was externally provided.

This is intended for externally supervised foreground harness processes where
the supervisor owns the socket. It is not used by the normal CLI-managed
runtime-dir harness path.

## Direct `tau component harness`

Running `tau component harness` directly starts the harness component without the
terminal UI parent and binds its own runtime-dir socket.

This path is useful for debugging or embedding the harness component, but it
does not receive an initial UI over stdio unless `--initial-ui-stdio` is supplied
by the CLI-managed startup path.

## Embedded one-shot runs

Embedded helpers such as `run_embedded_message` do not create a daemon socket.
They construct a harness in-process, run one interaction, and shut it down.
Socket activation and runtime-dir attach discovery do not apply to embedded
one-shot runs.
