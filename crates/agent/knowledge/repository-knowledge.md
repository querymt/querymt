---
id: pilot-repository-knowledge
title: Repository knowledge delivery
description: How repository documentation is indexed, retrieved, and delivered as orientation and index-context cards.
tags: knowledge, retrieval, delivery, orientation, gate
knowledge_paths: ["crates/agent/src/knowledge/repository/**", "crates/agent/src/agent/repository_delivery.rs"]
knowledge_sources: ["crates/agent/src/knowledge/repository/gate.rs"]
---

# Repository knowledge delivery

Repository knowledge is off by default and enabled only by host settings. The documents
stay as Markdown in the repository; the index is a disposable SQLite cache under the
QueryMT config directory (`src/knowledge/repository/root.rs`, `store.rs`).

## Selection and refresh

`.qmt/knowledge.toml` selects documents and code (`manifest.rs`, `discovery.rs`).
`reconcile` (`reconcile.rs`) compares size, mtime and file identity first and hashes
content only on a mismatch or inside the racy-timestamp window, so a warm refresh reads
no unchanged content. `RepositoryService` (`service.rs`) serializes refresh per checkout
and serves whole generations only.

## Pull retrieval

`query.rs` ranks two lanes: navigation (exact path and symbol matches, file and purpose
rows) and documentation (BM25 over authored sections). `browse.rs` implements get, list
and stats. The `repo_knowledge_*` tools in `src/tools/builtins/repo_knowledge.rs` are thin
wrappers with bounded output.

## Automatic delivery

Two independent host-enabled stages push short cited cards through the typed hook
contribution path (`adapters.rs`, `src/agent/repository_delivery.rs`):

- **Orientation** runs at an accepted user prompt. `gate.rs` applies relevance gate v1:
  NFKC and lowercase, a 2 KiB cap, identifier splitting, at most 64 tokens, fixed
  stopword and generic-verb lists, and a pass when an exact indexed path or identifier is
  named, or two tokens match title/headings/tags/description, or three match the body.
  `orientation.rs` turns passing sections into cards and falls back to configured
  entrypoints on the first task.
- **Index context** runs after a successful built-in `index` call.
  `path_knowledge.rs` selects File/Purpose rows, `knowledge_sources` documents and
  `knowledge_paths` matches for exactly that file. The tool result is never changed.

`budget.rs` charges both stages to one per-turn allowance (two cards, 600 estimated
tokens, 4 KiB) in stable order and refuses cards already represented in effective
history. Delivery state is rebuilt from card provenance marks on resume and after
compaction, never from a cache. `diagnostics.rs` keeps local matched, delivered,
suppressed and fetched counters.

## Maintenance

`check.rs`, `scaffold.rs`, `acknowledge.rs`, `evidence.rs` and `export.rs` implement the
deterministic maintenance operations behind `examples/repository_knowledge.rs`. Evidence
hashes report drift, never truth.

Related: [execution.md](execution.md) for the hooks these adapters join, and
[sessions.md](sessions.md) for the effective-history and compaction rules delivery uses.
