---
id: pilot-sessions
title: Session storage and history
description: Session persistence traits, the SQLite backend, the three compaction layers, run leases and session location.
tags: sessions, storage, sqlite, compaction, pruning, leases
knowledge_paths: ["crates/agent/src/session/**"]
knowledge_sources: ["crates/agent/src/session/run_lease.rs"]
---

# Session storage and history

## Storage traits

`src/session/backend.rs` defines `StorageBackend`, which separates the command side from
the query side: `session_store()` (messages, model configuration, undo; trait in
`src/session/store.rs`), `event_journal()` (durable events) and an optional `view_store()`
(projections for observability views, `src/session/projection.rs`). Schedule and knowledge
storage are optional extras. Every query of a database-backed store must be scoped by
session ID so one session never reads another's data.

`SqliteStorage` (`src/session/sqlite_storage/`) implements all three: `session_store.rs`,
`view_store.rs`, `event_journal.rs`, with schema migrations in `migrations.rs`. The domain
entities (tasks, intents, decisions, progress, artifacts, delegations, schedules) are
defined in `src/session/domain.rs` and persisted by the `repo_*.rs` repositories.

## Three compaction layers

1. **Tool output truncation** cuts one tool result before it is stored (see the tool
   output limits in execution policy).
2. **Pruning** (`src/session/pruning.rs`) soft-deletes old tool outputs: the most recent
   40,000 tokens are protected, pruning happens only when at least 20,000 tokens are
   prunable, and the `skill` tool's outputs are never pruned.
3. **AI compaction** (`src/session/compaction.rs`) asks the model for a continuation
   summary when the context threshold is reached. `filter_to_effective_history` returns
   the messages from the last compaction onward; that suffix is what the model sees and
   what delivery state is rebuilt from.

## Run leases and abandoned runs

A local run holds an exclusive OS file lock on `<root>/<session_id>.lock`
(`src/session/run_lease.rs`). The operating system releases it when the process dies, so
an acquirable lease means no live process runs that session. `src/session/run_reconcile.rs`
closes a run that started but never completed, and whose lease is free, by journaling
`run_completed` with outcome `interrupted` while holding the lease.

## Where a session lives

`src/session/location.rs` resolves a session ID to exactly one authoritative location,
local storage or a remote bookmark, from durable data only. A local row and a remote
bookmark for the same ID is a typed `Conflict`, never silently resolved.

## Providers

`SessionProvider` (`src/session/provider.rs`) builds the model provider. It caches the
provider keyed by LLM config ID, and each turn's handle caches it again, so a model switch
takes effect on the next turn, not in the middle of one.

Related: [execution.md](execution.md) for when pruning and compaction run.
