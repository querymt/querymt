# Proposal

## Why

Coding sessions repeatedly rediscover repository structure and design constraints that could be shared across sessions and developers. QueryMT already has knowledge tools, portable `.agents/memories` import, and deterministic source extractors, but lacks an integrated, repository-scoped retrieval path with useful body retrieval, source citations, freshness checks, and a rebuildable index.

## What Changes

- Add opt-in repository knowledge backed by Git-trackable Markdown and a QueryMT-owned `.qmt/knowledge.toml` source manifest. SQLite/FTS5 is a disposable local index under the QueryMT config directory, not a file to commit or exchange with session history.
- Keep QueryMT-specific control files under the project `.qmt/` namespace, alongside existing `.qmt/commands` and `.qmt/skills`. Nothing is added to `.agents/`: the dotagents convention (dotagents.io) defines only `AGENTS.md`, `mcp.json`, and `skills/`, and QueryMT's implemented `.agents` protocol does not define knowledge manifests. Authored knowledge stays tool-neutral Markdown wherever it already lives, and existing `.agents/memories` files can be selected and read in place.
- Accept ordinary Markdown and existing dotagents memories through document adapters, using the existing protocol frontmatter conventions for optional metadata (ID, title, description, tags, lifecycle status, evidence selectors, path associations). Existing documentation needs no frontmatter conversion. Index bounded, citable sections while preserving file ownership, stable identities, and private/session memory isolation.
- Favor curated subsystem concepts and small linked indexes for progressive disclosure. Reuse authored titles/descriptions for catalogs and orientation previews; synthesize missing listings read-only.
- Reuse existing symbol/outline extractors to generate deterministic file and symbol records. Do not use LLMs to invent file purposes, invariants, or dependency semantics.
- Add dedicated repository tools (`repo_knowledge_query`, `repo_knowledge_get`, `repo_knowledge_list`, `repo_knowledge_stats`) instead of overloading the session-memory knowledge tools. Provide separate navigation and documentation retrieval lanes, relative path and section/symbol citations, and freshness diagnostics. Existing knowledge tools are unchanged.
- Keep index freshness cheap and correct: a file-metadata fast path with content hashing on any mismatch or ambiguous timestamp, rather than hashing the whole corpus before every query.
- Provide deterministic maintenance operations for indexing, checking references/evidence, rendering code maps, previewing/applying optional inventory scaffolds, and acknowledging evidence. Evidence acknowledgements are stored per document under `.qmt/knowledge/evidence/` to avoid a single merge-conflict hotspot, and hash normalized text so line-ending differences do not cause false drift.
- Add two complementary, opt-in automatic delivery paths: a user-prompt orientation trigger with a fully specified, versioned lexical relevance gate, and post-tool enrichment of successful `index` calls with file-associated knowledge. Both use the same repository service as explicit retrieval.
- Include a controlled `crates/agent` pilot comparing the current workflow, curated notes plus generated maps, and exhaustive per-file documentation, and separately comparing delivery modes on the same corpus.
- Deliver in five milestones with independent acceptance gates: (M1) manifest, discovery, and pull retrieval; (M2) deterministic code maps; (M3) evidence and maintenance; (M4) automatic delivery; (M5) pilot. M1 is the minimum shippable unit; later milestones may be split into follow-up changes before implementation without changing M1 contracts.
- Exclude qndx integration, embeddings/vector services, automatic session-memory publication, automatic LLM documentation generation, and a new recursive instruction-precedence engine. Open Knowledge Format (OKF) bundles, nested-YAML metadata, review/authorship declarations, expiry metadata, and claim-footnote attribution are deferred to a follow-up change once Markdown-based repository knowledge has proven useful; the adapter boundary keeps that addition non-breaking.

## Capabilities

### New Capabilities

- `portable-repository-knowledge`: `.qmt/knowledge.toml` source manifest, portable document identity, safe discovery, checkout-local indexing, refresh, and private-memory separation.
- `repository-knowledge-retrieval`: Dedicated repository query/list/stats/get tools, ranked retrieval lanes, bounded citations, authorization, pre-discovery orientation triggers, post-index hook enrichment, and context lifecycle integration.
- `deterministic-code-maps`: Reproducible source-derived file/symbol records and compact maps using existing extraction infrastructure.
- `repository-knowledge-maintenance`: Per-document evidence freshness, reference/coverage checks, non-destructive scaffolding, and reproducible evaluation of documentation strategies.

### Modified Capabilities

None. The existing `index-tool` contract is reused without changing its output. The `dotagents-protocol` capability (`openspec/specs/dotagents-protocol/spec.md`) is unchanged: this change reads selected workspace memory files as an additional projection, adds no files or semantics to `.agents/`, and does not alter protocol layering, imports, or opt-in behavior.

## Impact

- Agent knowledge module, SQLite/FTS utilities, new repository knowledge tools, tool context and execution context, workspace/session lifecycle, configuration/builders, prompt assembly, prompt/tool hook contribution paths, and compaction-aware delivery state.
- Reuse the existing dotagents frontmatter parser conventions, filesystem-confinement helpers, symbol/outline extraction, QueryMT config-directory resolution, and workspace file-change facilities. No new YAML parser.
- A typed maintenance API with an executable agent example, developer documentation, fixtures, and opt-in pilot corpus/configuration for `crates/agent`.
- Additive opt-in configuration and new tools; existing knowledge tools and disabled configurations retain their behavior. No qndx dependency and no required model/provider service for index construction, retrieval, or maintenance.
- Coordinate checkout identity with the planned `add-git-worktree-isolation` change, without depending on that change being implemented. Existing unrelated changes are not modified.
