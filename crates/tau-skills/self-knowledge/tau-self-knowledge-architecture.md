---
name: tau-self-knowledge-architecture
description: >
  Use this skill when the user asks how Tau is architected, including the harness
  daemon, UI clients, Unix socket connections, agents, event flow, model
  orchestration, tool routing, or extension processes.
advertise: false
---

# Tau architecture overview

Tau runs as a daemon-centered system.

Typically, a UI process starts (or attaches to) a harness daemon for the current project. The UI sends user input and renders streamed events, while the harness owns orchestration.

The harness process:

- manages agent state and event flow,
- starts and supervises extension processes,
- routes tool calls to the right extension,
- records durable agent transcripts and per-run debug logs,
- builds agent prompts and handles model responses.

Extensions are separate processes connected to the harness. They register tools and capabilities, then handle requests from the harness and return results/events. This separation keeps tool/runtime concerns outside the core harness loop.

Clients connect to the harness over a Unix socket. This allows multiple clients to observe or interact with the same running harness while the harness remains the single coordinator.
