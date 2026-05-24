---
name: tau-self-knowledge
description: >
  Use this skill when the user asks about the Tau coding agent they are running
  in, including what Tau is, how it works, built-in self-knowledge, configuration,
  debugging, source code, community links, or where to find Tau-specific help.
advertise: true
---

# Tau self-knowledge

Tau is a coding agent harness.

To enable self-help it includes a built-in repository of skills with information about Tau itself.

## Build information

You are running inside Tau version `__TAU_SELF_KNOWLEDGE_VERSION__`, git revision `__TAU_SELF_KNOWLEDGE_HASH__`, built on `__TAU_SELF_KNOWLEDGE_BUILD_DATE__`.

## Built-in self-knowledge skills

- `tau-self-knowledge` — overview of built-in Tau-specific skills.
- `tau-self-knowledge-architecture` — high-level overview of Tau architecture and core components.
- `tau-self-knowledge-config` — directories, important config files, and provider setup commands.
- `tau-self-knowledge-email` — secure configuration for the built-in `std-email` extension.
- `tau-self-knowledge-source-code` — where to fetch Tau source code for debugging or detailed understanding.
- `tau-self-knowledge-community` — places to ask questions or talk about Tau.
- `tau-self-knowledge-debugging` — debugging workflow for Tau sessions, daemon behavior, logs, state, and provider request captures.

When working _on_ Tau project, prefer the repository's local developer-centric skills when available.
