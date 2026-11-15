---
id: pilot-agent-map
title: querymt-agent knowledge map
description: Entry point to the reviewed concept notes for the agent runtime, session storage, the tool system and repository knowledge.
tags: map, orientation, agent
---

# querymt-agent knowledge map

Reviewed concept notes for selected subsystems of the agent crates (`querymt-agent` was
split into `agent-core`, `agent-index`, `agent-knowledge`, `agent-sqlite`,
`agent-services` and `agent-runtime`). Each note explains how the subsystem works and
which invariants matter, then points at source. Read the note first, then open only the
files it names.

| File | Purpose |
|------|---------|
| [execution.md](execution.md) | How one prompt turns into model requests, tool calls, compaction and a final stop (`crates/agent-runtime/src/agent/execution`). |
| [tools.md](../../agent-services/knowledge/tools.md) | How tools are defined, normalized, validated, scheduled and permitted (`crates/agent-services/src/tools`). |
| [sessions.md](../../agent-services/knowledge/sessions.md) | How sessions, events and history are stored, pruned, compacted and leased (`session/` in `agent-core`, `agent-runtime`, `agent-services` and `agent-sqlite`). |
| [repository-knowledge.md](../../agent-knowledge/knowledge/repository-knowledge.md) | How repository knowledge is wired into prompts and `index` results (`crates/agent-knowledge`, `crates/agent-runtime/src/agent/repository_delivery.rs`). |

Conventions used by the notes:

- Every path in backticks is relative to the repository root.
- A "Source" line lists the files a claim rests on, so a reader can verify it.
- These notes are advisory evidence. They never override `AGENTS.md` or the code.
