# Proposal

## Why

Coding sessions repeatedly rediscover repository structure and design constraints that could be shared across sessions and developers. QueryMT already has knowledge tools, portable `.agents/memories` import, and deterministic source extractors, but lacks an integrated, repository-scoped retrieval path with useful body retrieval, source citations, freshness checks, and a rebuildable index.

## What Changes

- Add opt-in repository knowledge backed by Git-trackable Markdown and a portable `.agents/knowledge.toml` source manifest. SQLite/FTS5 is a disposable local index, not a file to commit or exchange with session history.
- Index selected memories, architecture documents, and directory `AGENTS.md` documents into bounded, citable sections. Preserve file ownership, stable identities, and private/session memory isolation.
- Reuse existing symbol/outline extractors to generate deterministic file and symbol records. Do not use LLMs to invent file purposes, invariants, or dependency semantics.
- Add explicit repository selection to knowledge query/list/stats tools and a bounded `knowledge_get` tool. Provide separate navigation and documentation retrieval lanes, with relative paths, section/symbol references, and freshness diagnostics.
- Add two complementary, opt-in automatic delivery paths: a user-prompt orientation trigger that provides relevant repository pointers before any source path is known, and post-tool enrichment of successful `index` calls with file-associated knowledge. Use the same repository service as explicit retrieval, not a second memory bank.
- Keep the structural index output unchanged and attach bounded, provenance-labeled knowledge through the hook context channel. Retain a short stable retrieval policy, shared delivery budgets, duplicate suppression, and compaction-aware re-evaluation; do not rewrite system prompts per turn, block source tools, or inject the whole corpus.
- Provide deterministic maintenance operations for indexing, checking references/evidence, rendering code maps, and previewing/applying optional per-directory file-table scaffolds. Preserve authored content; never automatically write semantic descriptions or mark changed evidence reviewed.
- Include a controlled `crates/agent` pilot comparing the current workflow, curated notes plus generated maps, and Pi-style exhaustive per-file documentation. Measure navigation quality, discovery cost, correctness, and maintenance effort before choosing repository-wide documentation coverage. Separately compare pull-only retrieval, index enrichment, and index enrichment plus pre-discovery triggers on the same corpus.
- Exclude qndx integration, embeddings/vector services, automatic session-memory publication, automatic LLM documentation generation, and a new recursive instruction-precedence engine.

## Capabilities

### New Capabilities

- `portable-repository-knowledge`: Source manifest, portable document identity, safe discovery, checkout-local indexing, refresh, and private-memory separation.
- `repository-knowledge-retrieval`: Explicit repository-aware query/list/stats/get contracts, ranked retrieval lanes, bounded citations, authorization, pre-discovery orientation triggers, post-index hook enrichment, and context lifecycle integration.
- `deterministic-code-maps`: Reproducible source-derived file/symbol records and compact maps using existing extraction infrastructure.
- `repository-knowledge-maintenance`: Evidence freshness, reference/coverage checks, non-destructive scaffolding, and reproducible evaluation of documentation strategies.

### Modified Capabilities

None. The existing `index-tool` contract is reused without changing its output. The completed but unarchived `add-dotagents-protocol-support` change remains authoritative for legacy protocol imports; this change adds a separate repository projection without changing its opt-in behavior or global/workspace merge rules.

## Impact

- Agent knowledge module, SQLite/FTS utilities, built-in knowledge tools, tool context and execution context, workspace/session lifecycle, configuration/builders, prompt assembly, prompt/tool hook contribution paths, and compaction-aware delivery state.
- Existing dotagents frontmatter parsing and filesystem-confinement helpers, plus symbol/outline extraction and workspace file-change facilities.
- A typed maintenance API with an executable agent example, developer documentation, fixtures, and opt-in pilot corpus/configuration for `crates/agent`.
- Additive opt-in configuration and tool parameters; existing session-scoped knowledge calls and disabled configurations retain their behavior. No qndx dependency and no required model/provider service for index construction, retrieval, or maintenance.
- Coordinate checkout identity with the planned `add-git-worktree-isolation` change, without depending on that change being implemented. Existing unrelated changes are not modified.
