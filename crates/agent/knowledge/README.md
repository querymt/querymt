---
id: pilot-agent-map
title: querymt-agent knowledge map
description: Entry point to the reviewed concept notes for the agent runtime, session storage and tool system.
tags: map, orientation, agent
---

# querymt-agent knowledge map

Reviewed concept notes for three subtrees of `crates/agent/src`. Each note explains
how the subsystem works and which invariants matter, then points at source. Read the
note first, then open only the files it names.

| File | Purpose |
|------|---------|
| [execution.md](execution.md) | How one prompt turns into model requests, tool calls, compaction and a final stop (`src/agent/execution`). |
| [tools.md](tools.md) | How tools are defined, normalized, validated, scheduled and permitted (`src/tools`). |
| [sessions.md](sessions.md) | How sessions, events and history are stored, pruned, compacted and leased (`src/session`). |
| [repository-knowledge.md](repository-knowledge.md) | How repository knowledge is wired into prompts and `index` results (`src/knowledge/repository`, `src/agent/repository_delivery.rs`). |

Conventions used by the notes:

- A path in backticks is relative to `crates/agent/` unless it starts with `crates/`.
- A "Source" line lists the files a claim rests on, so a reader can verify it.
- These notes are advisory evidence. They never override `AGENTS.md` or the code.
