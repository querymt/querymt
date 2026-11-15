# Design

## Context

See proposal.md for motivation and scope. This is a cross-cutting configuration, storage, retrieval, and workspace-lifecycle change.

Observed integration points:

- `crates/agent/src/knowledge/mod.rs:277` defines session-oriented ingest/query/consolidation and protocol reconciliation. `crates/agent/src/knowledge/sqlite.rs:990` implements lexical retrieval with metadata boosts, not embeddings.
- `crates/agent/src/tools/builtins/knowledge_query.rs:104` selects the session scope by default; the returned entries omit their raw body. `knowledge_list.rs:91` lists unconsolidated entries, not a document corpus.
- `crates/agent/src/dotagents/mod.rs:92` derives protocol scope from a canonical workspace path. Protocol imports support stable source keys, fingerprints, and inactive records. Their summaries can be only the title (`dotagents/memory.rs:144`). `dotagents/parsers.rs` parses frontmatter scalars, quoted values, comma-separated lists, and JSON-array lists.
- Project-level QueryMT configuration already uses `<root>/.qmt/` (`slash_commands/discovery.rs:35` for `.qmt/commands`; `.qmt/skills` in `openspec/specs/dotagents-protocol/spec.md`). The user-level config directory is resolved by `querymt_utils::providers::config_dir()`.
- `crates/agent/src/index/symbol_index/types.rs:111` already provides names, signatures, ranges, parent/child structure, and digests. Outline and symbol extractors cover Rust and other languages; reuse them rather than maintaining another parser.
- `crates/agent/src/index/workspace_actor.rs:277` processes file changes. Existing watchers are useful invalidation hints, not evidence that no external write was missed.
- `crates/agent/src/tools/context.rs:39` and `agent/execution_context.rs:141` carry tool capabilities and knowledge access. Session materialization and execution context must supply the effective checkout, not a fixed builder cwd.
- `crates/agent/src/hooks/engine.rs:974` accepts post-tool `additional_context` with handler and tool-call provenance. `agent/execution/tool_calls.rs:1119` persists contributions after all tool-result blocks in the combined message, preserving call/result adjacency. User-prompt context contributions and the post-compaction lifecycle are also available.
- The single-coder profile does not expose knowledge tools (`examples/confs/single_coder.toml:21`). Prompt policy alone cannot fix an unavailable tool or incorrect scope.
- Pi's `packages/kb/src/dox.ts:1` explicitly separates deterministic inventory/audit from LLM-authored purposes. Its extension adds retrieval lanes, body fetch, reindexing, and freshness diagnostics; these ideas do not require copying its mandatory lookup-before-every-read rule.

Specification compatibility:

- Preserve `openspec/specs/index-tool/spec.md`, including byte-identical existing outline formatting.
- `dotagents-protocol` (archived change `2026-09-28-add-dotagents-protocol-support`, now `openspec/specs/dotagents-protocol/spec.md`) remains authoritative for `.agents/` discovery, layering, and memory import. This change adds no files to `.agents/` and does not modify that capability. Repository knowledge may *read* selected `.agents/memories/*.md` files as an additional projection.
- `add-git-worktree-isolation` is still a proposal. Accept an explicit checkout root from future workspace bindings, but support ordinary repositories and linked worktrees without depending on its new APIs.

## Goals / Non-Goals

**Goals:**
- Reduce model-side orientation work, not merely filesystem search latency.
- Let a fresh clone reconstruct useful knowledge without another developer's SQLite database or session history.
- Separate source-derived facts, authored explanations, and private memories in both storage and output.
- Support incremental reuse with explicit freshness, bounded retrieval, and reproducible maintenance.
- Ship a useful pull-retrieval slice (M1) before the more speculative stages.

**Non-Goals:**
- qndx, vectors/embeddings, a graph database, whole-program semantic resolution, or inferred call/dependency graphs.
- Automatic LLM summaries, automatic knowledge publication from sessions, background commits, or automatic edits to authored notes.
- New recursive AGENTS instruction enforcement or blocking all source reads until a KB query succeeds.
- A production-wide exhaustive documentation mandate before evaluation.
- A generic trigger language, command/error blocking guards, automatic trigger extraction from prose, classifier-based relevance gates, web/session memory consolidation, or retrospective lesson mining.
- Defining a cross-tool standard. Control files are QueryMT-specific and are namespaced as such.
- In this change: OKF bundle support, a general nested-YAML reader, review/authorship declarations, expiry metadata, and claim-footnote attribution (see Deferred Work).

## Milestones

| Milestone | Scope | Gate before next milestone |
|---|---|---|
| M1 | Settings, `.qmt/knowledge.toml`, discovery, Markdown/memory adapters, local index, refresh, `repo_knowledge_*` tools, path-level navigation | Clone/rebuild equivalence, confinement, privacy, worktrees, edits/removals, malformed siblings, bounded output, unchanged legacy tools |
| M2 | Symbol-level code maps, Rust doc extraction, map rendering/export | Deterministic exports, unchanged index snapshots |
| M3 | Per-document evidence, checks, scaffolding, acknowledgement, maintenance example | Deterministic diagnostics, preimage-checked writes |
| M4 | Orientation and post-index delivery, budgets, dedup, compaction | Negative-case replay, output-policy preservation, retry/resume dedup |
| M5 | `crates/agent` pilot and evaluation | Documented results/limitations |

M1 is independently shippable. M2–M5 may be split into follow-up changes before implementation; doing so must not change M1 contracts.

## Decisions

### 1. QueryMT-owned manifest in `.qmt/`; Markdown is authoritative

Add host-controlled `repository_knowledge` settings to standalone/quorum configuration and builders. The feature is disabled by default. Host settings enable it, optionally provide a root, constrain allowed source roots, and set size/output budgets. A repository file cannot enable the feature, expand host permissions, or grant tools.

Read `<root>/.qmt/knowledge.toml` when enabled. v1 shape:

```toml
version = 1

[documents]
include = [".agents/memories/*.md", "docs/architecture/**/*.md", "crates/agent/**/AGENTS.md"]
exclude = ["openspec/changes/archive/**"]

[code]
include = ["crates/agent/src/**/*.rs"]
exclude = []

[orientation]
entrypoints = ["docs/architecture/README.md#Repository map"]
```

Paths/globs are relative to the selected root; excludes win. Missing manifest means diagnostic `not_configured`, not an implicit repository-wide crawl. An explicit maintenance preview can propose a manifest but cannot create it without an apply action. Unknown manifest versions and unknown top-level tables are rejected with diagnostics so a future format is never half-interpreted. Host limits cap files, bytes per file, and total indexed bytes; skipped sources are disclosed as incomplete coverage rather than an exhaustive answer.

**Location rationale.** The manifest and evidence records are only meaningful to QueryMT. The dotagents convention (dotagents.io) standardizes `.agents/AGENTS.md`, `.agents/mcp.json`, and `.agents/skills/` only; QueryMT's implemented `.agents` protocol adds memories, agents, tasks, models, and prompts, but no knowledge manifest. Placing QueryMT-only files in `.agents/` would imply cross-tool portability that does not exist, risk collisions if the shared convention later defines `knowledge*` differently, add noise for tools that sync or scan `.agents/`, and could surface as unknown-file diagnostics in the protocol loader. `.qmt/` already holds project-level QueryMT configuration (`.qmt/commands`, `.qmt/skills`). Portability comes from the authored Markdown, which stays wherever it lives and is readable by any tool. If a cross-tool knowledge convention emerges, a later change can accept it as an additional manifest location.

Alternatives considered: `.agents/knowledge.toml` (rejected for the reasons above); a key in a root `querymt.toml` (rejected: no such project-level file exists and the manifest needs its own version); host-only configuration without a repository file (rejected: the corpus selection must travel with the repository to make clones reproducible).

**Adapters.** Use separate adapters for ordinary Markdown and workspace dotagents memories. Workspace memories retain legacy parsing and enabled/id/title/tags semantics. Ordinary Markdown needs no frontmatter; when frontmatter is present it is parsed with the existing dotagents frontmatter conventions (scalars, quoted values, comma-separated and JSON-array lists), and unknown keys are retained and ignored. Recognized optional keys:

- `id`: explicit stable identity surviving moves.
- `title`, `description`, `tags`: catalog and card text.
- `status`: open-vocabulary lifecycle label; `draft` and `deprecated` are recognized for delivery eligibility, absent means usable.
- `knowledge_sources`: explicit, hash-checkable evidence dependencies as `path` or `path#qualified::symbol` selectors.
- `knowledge_paths`: repository-relative globs for advisory applicability; they never certify freshness or truth.

Explicit IDs survive moves; otherwise identity derives from repository-relative path. Duplicate explicit IDs disable all conflicting documents with diagnostics. Malformed frontmatter excludes that document with a diagnostic; siblings remain usable.

**References.** Resolve relative Markdown links from the containing document, normalize them, and enforce checkout confinement before any read. Links never select additional index inputs or grant reads. External URLs are metadata only: indexing/checking performs no network requests. Markdown links (related material), `knowledge_sources` (evidence dependencies), and `knowledge_paths` (applicability) stay distinct.

**Confinement.** All reads validate canonical confinement and reject traversal, external symlinks, and non-regular sources. Git-ignored content is excluded by default, even when a broad glob matches; an explicit host allow rule is required to include it. Selected hidden documentation such as `.agents/memories` is allowed. There is no ambient scan of home directories, global protocol memories, session stores, or unrelated worktrees.

Alternative: exporting session SQLite mixes private data with derived artifacts and makes review/versioning poor. A separate proprietary document format duplicates Markdown and current protocol parsing.

### 1.1. Curated concepts and progressive disclosure are the primary authoring model

Favor small subsystem explanations, invariants, decisions, and playbooks connected by ordinary Markdown links. Authored `description` values supply catalog previews and orientation descriptions; fall back to bounded verbatim headings/excerpts when absent, never model-generated summaries. Optional per-directory index documents let readers descend from repository orientation to subsystem concepts and then source citations without QueryMT tooling.

Consume authored indexes as citable navigation material. When a directory has no authored listing, synthesize a bounded listing from selected documents' titles/descriptions in memory; no write occurs and synthetic listings are labeled derived with citations to the underlying documents. Do not replace authored listings, inject whole trees, or infer evidence dependencies from index links. Synthesis alone does not enable orientation fallback. Exhaustive File/Purpose tables remain an optional pilot variant.

### 2. Checkout-local derived store, within the existing knowledge module

Add a repository-knowledge service and repository-store interface under `src/knowledge/`. Keep the legacy `KnowledgeStore` trait, retention, consolidation, and protocol-owned tables unchanged. Reuse SQLite/FTS patterns and text utilities where appropriate, but give repository documents their own schema and lifecycle.

Use a rebuildable database at `<config_dir>/cache/knowledge/<checkout-key>/index.sqlite`, where `<config_dir>` is resolved by the shared QueryMT config-directory helper (default `~/.qmt`) and is host-overridable. It is never copied into Git or embedded into `sessions.db`. A checkout key incorporates the canonical checkout root, selected knowledge root, and index/config version. Shared Git common-dir identity alone is insufficient: linked worktrees must not overwrite each other's active corpus. Public citations and portable IDs never contain this machine-local key.

Suggested tables: index generations/config metadata; sources (relative path, kind, stable document ID, size, mtime, content hash); chunks (heading/symbol identity, ranges, body, tags); code records; evidence dependencies; FTS tables. Store authored documents separately from generated navigation records. Stable chunk keys use document ID plus heading breadcrumb and occurrence, or file path plus qualified symbol, kind, and occurrence. Heading/symbol renames can invalidate keys; `repo_knowledge_get` must report that rather than return a different section.

Workspace `.agents/memories` can still be imported by the existing protocol system. The repository projection reads those files directly and deduplicates overlapping manifest patterns. Repository queries never union the protocol/session tables, so coexistence does not produce duplicate repository hits or leak global overrides. Deleting a repository document affects only the derived repository index. Repository chunks are not consolidation inputs.

Alternative: changing default scopes or using `deactivate_protocol_sources` for all documents risks silently altering legacy imports and deleting records owned by another loader.

### 3. Root selection and refresh

Root precedence: host-provided effective checkout/knowledge root; otherwise discover the Git worktree root containing the session cwd; otherwise require an explicit root for non-Git workspaces. The cwd must lie within the selected root. Do not use the nearest Cargo manifest as repository identity: starting at `crates/agent` should find the same root manifest as starting at repository root. Nested Git repositories select their own worktree unless the host explicitly supplies a containing authorized root.

Bind the repository service to each effective session workspace and share it across sessions on the same checkout/configuration. No repository access exists when cwd/root is unavailable, on a remote filesystem not mounted on the executor, or when policy denies the selected root.

**Freshness fast path.** Before serving a retrieval, enumerate configured inputs and compare each file's `(size, mtime_ns, inode/file-id)` against the stored record. Hash content only when any of these differ, when the stored mtime is within a racy-timestamp window of the previous index time (Git's "racily clean" rule), or when the platform cannot provide reliable metadata. Re-parse only files whose hash changed. Manifest changes and enumeration changes (additions/deletions) always reconcile. Watcher events coalesce work and accelerate invalidation, but are not the only correctness mechanism. A host setting can force full hashing for debugging. Profile cold/warm cost in the pilot.

Reconcile additions, modifications, disabled/deleted sources, manifest changes, and checked-out branch contents. Changes apply transactionally to an index generation. Serialize refresh per checkout and use SQLite transactions/busy handling for multiple processes. Readers receive a complete generation, never partially rebuilt FTS state. Recheck the selected source's metadata (and hash on mismatch) before emitting bodies/coordinates; bounded retries handle concurrent edits, then return an explicit updating/unavailable diagnostic. This is not a filesystem snapshot against arbitrary external writers.

If a source becomes invalid, do not present its previous body as current. Exclude that source from live results and report a source-specific diagnostic while valid siblings remain usable. Global refresh failure or cache corruption produces an unavailable diagnostic and normal-code-search guidance; do not silently answer from stale cache. The cache can be rebuilt without touching authoritative files or private knowledge.

### 4. Deterministic structure supplements authored meaning (M2)

M1 provides path-level navigation records (relative path, language, size) so navigation is useful before symbol extraction. M2 reuses `SymbolIndex` and the shared outline extraction pipeline to persist compact records containing path, language, kind, qualified name, signature, parent, ranges, and digest. Existing source documentation can be copied verbatim with source coordinates; first version supports Rust module/item doc comments.

Represent declared imports as syntax, not resolved semantic dependencies. Unsupported/unparseable files remain visible as path-only records with coverage diagnostics. Generated facts never claim a semantic purpose from a filename. Source and extractor/schema fingerprints invalidate cached records. Rendering and serialized map export use stable relative-path/symbol ordering without timestamps or machine-local paths.

Expose directory/file maps on demand through repository navigation retrieval and maintenance `map`. Generated maps remain local by default. An explicit export can produce a portable Markdown map; regenerating it replaces only a designated generated artifact or managed block, never prose in an arbitrary AGENTS file. Existing `index`, `get_symbol`, and edit precondition behavior stays unchanged.

### 5. Dedicated repository tools; legacy knowledge tools unchanged

Add four tools rather than a `collection` switch on existing ones:

- `repo_knowledge_query { question, lane?, path?, limit? }`: `lane` is `navigation`, `documentation`, or `all` (default).
- `repo_knowledge_get { id? | path + section?, cursor? }`.
- `repo_knowledge_list { path?, cursor? }`: paginated document/navigation catalog.
- `repo_knowledge_stats {}`.

Rationale: overloading `knowledge_query` would require rejecting legacy parameters (`scope`, `retrieval_mode`, `include_consolidations`) in repository mode and give `knowledge_list` two meanings. Separate tools keep legacy schemas byte-for-byte unchanged, give models a clear contract, and let profiles allowlist repository knowledge independently from session memory.

Authorization validates the effective checkout on every operation; callers cannot select another checkout by supplying an ID or path. Navigation ranks exact path/qualified-name/name matches and generated facts/file-table rows. Documentation ranks authored heading/body matches using FTS5 BM25 and deterministic tie-breaking. `all` merges deduplicated results under a shared budget. Treat query text as user text, never raw FTS syntax; empty input has a clear error. No model call or external service is needed.

Results contain a repository-local retrievable ID, kind/provenance, relative source path, original line range, section/symbol selector, bounded excerpt, and index/evidence/lifecycle state. Source-derived facts and authored descriptions remain visibly distinct. Default result limit is 10, hard maximum 50. Default serialized response budget is 16 KiB, host-capped at 64 KiB; report truncation and how to narrow/fetch rather than silently omit.

`repo_knowledge_get` accepts either an ID or a relative path plus optional section selector, with bounded pagination. A path-only multi-section request returns a section catalog and preamble, not an arbitrary first section disguised as the whole document. Fetches revalidate checkout authorization, current source identity, and freshness. `repo_knowledge_list` pagination discloses generation changes. `repo_knowledge_stats` reports coverage, active source/chunk counts, index version/generation, and diagnostic/freshness counts. Existing ingest/consolidate operations remain local-memory operations and cannot mutate repository files.

### 6. Two delivery stages backed by one repository service (M4)

A post-index hook alone cannot solve initial orientation: an unfamiliar agent does not yet know which file to index. Keep explicit query/get available and implement both automatic stages below. Host `repository_knowledge.delivery` settings independently enable `orientation` and `index_context`; both default off. Repository metadata can describe associations but cannot enable handlers, grant access, or supply executable hook commands.

```text
User task without a known source path
  --> orientation trigger --> ranked subsystem/source pointers
  --> agent chooses index(path)
  --> structural outline + attributed knowledge-for-path digest
  --> focused repo_knowledge_get/source read --> edit with current-source checks
```

#### 6.1. Prompt-triggered orientation before file selection

At accepted user-prompt submission, before the first model request for that turn, perform bounded local lexical retrieval using the user's task text. This does not depend on a path, symbol, previous tool call, or model-issued knowledge request. No LLM is called and no triggers are inferred from arbitrary prose.

The relevance gate is versioned (`gate_version = 1`) and fully specified so it can be frozen before held-out evaluation:

1. Normalize the prompt: Unicode NFKC, lowercase, truncate to 2 KiB.
2. Tokenize: split on non-alphanumeric characters except `_`; additionally split identifier tokens on `::`, `.`, `/`, `-`, `_`, and camelCase boundaries, keeping the original identifier as an extra token. Cap at 64 distinct tokens.
3. Drop tokens shorter than 3 characters, pure numbers, and tokens in a fixed, compiled-in English stopword list plus a small fixed list of generic coding verbs (e.g. "add", "fix", "make", "update", "change", "implement", "code", "file", "function"). Both lists are versioned with the gate.
4. A document section passes when either (a) the prompt contains an exact repository-relative path or qualified identifier present in the index, or (b) at least two distinct remaining tokens match the section's title, headings, tags, or description, or at least three match its body.
5. Rank passing sections with the documentation-lane BM25 ranking and deterministic tie-breaks.

Non-English prompts mostly fail step 4b and produce no card; this is intended conservatism, not a bug. The gate is an explainable heuristic, not a calibrated confidence score; false positives are measured by replay.

For the first accepted task in a session, and once after compaction if needed, the manifest `[orientation] entrypoints` list can supply a small reviewed repository map when no gated result exists. Values are stable document IDs or relative document paths with optional section selectors, not executable rules. Missing configuration means no fallback card; invalid entrypoints produce diagnostics rather than a repository-wide dump.

Append selected cards as clearly attributed user-prompt context through the existing contribution pipeline, not by replacing the system prompt. A card contains an authored short description or verbatim bounded excerpt/heading, current relative citations, source pointers where available, and evidence state. It identifies candidate entry points, not a mandatory edit target.

#### 6.2. Successful index calls receive knowledge-for-path context

At the post-tool stage for the built-in `index`, resolve the actual successful indexed file with the same root/path semantics as the tool, including effective inputs after any rewrites. Do not trust tool name alone for an MCP tool with the same name. Confirm the canonical file is in the authorized selected corpus. Match direct File/Purpose rows, `knowledge_sources` dependencies, and `knowledge_paths` associations. No fuzzy basename matching and no searching result text for incidental path mentions.

Return a compact digest through typed hook contributions tied to the tool-call ID and indexed relative path. Preserve the original structural outline byte-for-byte and do not patch `updated_output` or its error flag. Prefer authored purpose, relevant non-obvious invariants, and architecture/test pointers; do not duplicate the outline's signatures. A digest is a deterministic selection of existing content. Label changed/unverified evidence explicitly; missing evidence yields only a cautionary pointer.

Skip failed/denied index calls, out-of-corpus files, absent knowledge, and exhausted delivery budgets. Enrichment errors/timeouts leave the successful index result intact and emit a rate-limited host diagnostic. The provider invokes the repository service directly, not model-visible tools, avoiding recursive hooks and extra model round trips.

#### 6.3. Native lifecycle adapters reuse hook contribution semantics

Implement the two selectors as shared repository-service operations and attach in-process adapters at user-prompt and post-tool contribution stages. This is new compiled integration; the current configurable hook engine supports command/MCP handlers, not a native callback registration API. Reuse the typed provenance and message assembly path without introducing a general hook language. The maintenance example can offer a hook-JSON adapter for trusted command-hook deployments using the same operations. Enable only one adapter per delivery stage.

The index handler is observational: it neither changes permission decisions nor alters execution facts. Run it only after successful execution and applicable output-policy processing; a suppressed/redacted result must not regain restricted information through enrichment. Bound delivery time; unready refresh or selector failure skips automatic context rather than delaying the turn or serving known obsolete data.

#### 6.4. Shared budgets, duplicate suppression, and compaction

Both stages share a per-turn budget, initially at most two cards and 600 estimated tokens, with a 4 KiB serialized hard ceiling including labels/citations. Host settings can lower these defaults; increasing them is bounded by a fixed implementation maximum. A card that cannot fit is omitted, never marked delivered. Allocate parallel index-call cards in stable tool-call order.

Track delivered card ID, card/source/evidence version, session, checkout, and compaction epoch. Deduplicate across channels and repeated tool calls while the same version is represented in effective context. Update delivery state only when the contribution is included in the committed request. Provider retries reuse the prepared context without rerunning selection. Resume reconstructs delivery state from contribution provenance and the effective compaction boundary.

On compaction, re-evaluate a bounded set of recently relevant cards using the active task and recently indexed paths. Revalidate sources/evidence and attach retained cards through post-compaction context or the next request. Do not replay every historical card; dropped/changed documents cannot be resurrected from stale text.

Record local matched/delivered/suppressed counts, reasons, added tokens, and latency without writing statistics or raw prompts into shared documents. 'Delivered' and 'fetched' are observable events; neither implies 'followed'.

#### 6.5. Stable policy and trust boundary

When `repo_knowledge_query`/`repo_knowledge_get` are allowed, attach one compact policy explaining retrieval lanes, advisory cards, relevant-section fetch, current-source verification, and normal-search fallback. Do not mandate a lookup before targeted reads. Automatic delivery is separately host-authorized and does not implicitly enable tools; when get is unavailable, cards use navigable relative citations without instructing an unavailable tool call.

Keep the system policy stable across turns; variable cards travel in attributed contextual contributions. Directory AGENTS content remains repository evidence, not higher-priority instructions. Never inject the entire corpus, private memory, arbitrary repository commands, or repeated policy copies.

Alternatives rejected: only post-index enrichment misses the unknown-file problem; only a 'search first' prompt relies on voluntary retrieval; a broad learned/regex trigger engine adds false positives.

### 7. Separate index freshness from evidence freshness (M3)

Index freshness means indexed bytes match current files. Evidence freshness means the source supporting an authored statement matches the last explicit acknowledgement. Neither proves the statement is true.

Store acknowledgements as one Git-trackable file per acknowledged document at `.qmt/knowledge/evidence/<document-path>.json`, mirroring the document's repository-relative path (documents with an explicit `id` still use their current path; moving a document moves its evidence file, and `check` reports orphaned evidence files). Per-document files avoid a single merge-conflict hotspot when different branches acknowledge different notes. Each file has a `version`, and entries per section or file-table row containing the reviewed-text hash and dependency selectors with versioned SHA-256 hashes, serialized with sorted keys and a trailing newline.

**Hash normalization.** Text hashes (reviewed sections and dependency files) are computed after: stripping a UTF-8 BOM, converting CRLF/CR to LF, and removing trailing whitespace on each line. Symbol hashes use the extractor's existing digest. The normalization is part of the hash version, so CRLF clones and editor whitespace differences do not cause false drift.

Dependencies come from declared `knowledge_sources` or the file path represented by a file-table row; use whole-file hashes first and symbol hashes only for unambiguous supported selectors. Missing/ambiguous selectors are diagnostics. Ordinary Markdown links are checked separately for broken references and do not imply evidence dependencies.

Evidence states are `unverified`, `unchanged`, `changed`, and `missing`. An edited note with no matching acknowledgement becomes unverified; changed dependency bytes are changed, not automatically false. A vanished dependency is missing. No confident rename is inferred from a similar filename. Acknowledgement is an explicit, previewable maintenance write bound to the exact note and dependency hashes, not a side effect of indexing, fetching, or generating maps. Evidence files contain no absolute paths, session IDs, timestamps required for equality, or private content.

Lifecycle `status` is independent of evidence: `draft`/`deprecated` documents stay explicitly retrievable with labels and are excluded from automatic cards.

### 8. Deterministic maintenance API and minimal executable surface (M3)

Expose typed service operations and an executable `crates/agent/examples/repository_knowledge.rs` wrapper, usable without a model/provider.

Example invocation: `cargo run -p querymt-agent --example repository_knowledge -- --root . check --json`.

Operations:

- `refresh`: reconcile/rebuild the local index, report coverage and failures.
- `map`: render bounded file/directory structure; explicit output option for generated artifacts.
- `check`: read-only diagnostics for source coverage, malformed/duplicate documents, broken local references, file-table orphans/missing rows, evidence drift, orphaned evidence files, invalid orientation entrypoints or `knowledge_paths`, and configured size budgets. Missing per-file rows are only findings in exhaustive-coverage mode. JSON output has stable codes/relative paths, and a failing check returns nonzero.
- `scaffold`: preview sorted directory file-table additions; `--apply` writes only newly created files or explicitly marked managed inventory blocks. Existing files without an unambiguous managed block are left untouched with a diagnostic. Never populate a purpose cell, remove/overwrite prose, prune orphan rows, or create a root-wide file table.
- `acknowledge`: preview evidence records for explicitly selected documents/rows; `--apply` writes per-document evidence files only after verifying preview preimages.

Apply operations fail on changed preimages and write atomically within the authorized root. There is no autonomous maintenance writer tool and no implicit Git commit.

**Managed blocks inside AGENTS files.** A managed inventory block is delimited by HTML comments (`<!-- qmt:inventory:begin -->` / `<!-- qmt:inventory:end -->`) so that other tools loading the file as instructions see an ordinary Markdown table with no QueryMT-specific prose. The block contains only a path/purpose table; QueryMT never writes instructions into it. Documentation recommends keeping inventories out of root `AGENTS.md` to avoid inflating every tool's instruction context.

### 9. Compare documentation strategies before adopting one (M5)

Build a small opt-in pilot under `crates/agent`, covering selected runtime, session, and tool subtrees. Do not create AGENTS files across all directories merely to demonstrate the feature.

Evaluation arms use the same source revision and tool budgets:

- Baseline: current source-navigation workflow.
- A: a small reviewed subsystem corpus, existing docs, and deterministic maps.
- B: the same corpus/maps plus exhaustive per-file purpose rows in the selected subtrees, with inventory scaffolding generated deterministically and descriptions reviewed before use.

For each fixed corpus, compare delivery modes independently: pull-only; pull plus index enrichment; and pull plus index enrichment plus orientation triggers. Include tasks with no file names or symbol identifiers. Replay unrelated prompts, generic terms, non-English prompts, external paths, failed index calls, parallel calls, and compaction/resume to measure false-positive rate, missed cues, duplicate delivery, added tokens, and latency. Freeze the relevance-gate version and word lists on development examples before held-out evaluation.

Include both file/symbol-location and conceptual questions. Freeze corpus authoring before held-out queries; historical session mining is optional, local-only, and requires sanitization. Deterministic retrieval fixtures run without a model; an opt-in end-to-end runner records model/version, repeated trials, tool/output tokens, latency, task correctness, and documentation authoring/update effort. Also record cold and warm refresh cost with the metadata fast path.

Mandatory correctness gates: no cross-session/private leakage; identical logical results after clone/rebuild; fresh edits/deletions/branch switches; deterministic maps; bounded output; unchanged legacy tools. Report results and limitations; the pilot does not enable exhaustive coverage globally.

## Deferred Work

Deferred to a follow-up change after M1 demonstrates value:

- Open Knowledge Format (OKF v0.2) bundles: `[[bundles]]` manifest entries, reserved `index.md`/`log.md` roles, bundle-root `/path` references, concept `type` requirement, and OKF identity mapping.
- A general bounded nested-YAML reader for metadata the dotagents frontmatter parser cannot express.
- Review/authorship declarations (`verified`, `generated`), `stale_after` expiry, `sources` provenance with claim footnotes, and inert executor/attester metadata.

The adapter interface, retained unknown frontmatter keys, and separate evidence model are designed so these can be added without changing M1 contracts. Unknown manifest tables are rejected in v1, so a future `[[bundles]]` table requires a manifest version bump or an explicit opt-in rather than being silently ignored.

## Risks / Trade-offs

- Metadata fast path can miss same-size, same-mtime edits -> racy-timestamp rule, inode/file-id comparison, hash on any doubt, and a forced-hash host setting; pre-emission revalidation of bodies.
- `.qmt/` is less discoverable to other tools -> acceptable: the control files are QueryMT-only; the authored Markdown remains tool-neutral and in place.
- Source structure is not program semantics -> label generated syntax honestly.
- Notes can be wrong even with unchanged evidence -> retain citations and source verification; hashes report drift, not truth.
- Per-document evidence files add many small files -> only acknowledged documents get one; check reports orphans after moves.
- Duplicate projections of workspace memories -> separate collection boundaries and deduplicate repository discovery.
- Repository-controlled instructions or secrets -> host opt-in, source confinement, explicit selection, ignored-file policy, and no elevation of retrieved content to system authority. Explicitly selected files can still contain secrets; document that review is required.
- Heading moves and duplicate names -> stable selectors include occurrence; stale IDs fail explicitly.
- Multi-process refresh or external edits -> transactional generations, per-checkout serialization, preimage checks, bounded retry, explicit unavailable state.
- Prompt retrieval can deliver irrelevant context -> specified conservative gate, optional reviewed entrypoints, small shared budgets, and negative-case replay before enabling it in a profile.
- Automatic enrichment can undo output policy or duplicate context -> only authorize selected sources, respect suppression/redaction, deduplicate across channels.
- Deferring OKF may require a later migration for teams that adopt it externally -> M1 accepts any Markdown, so OKF concept files are already indexable as plain Markdown; only bundle-specific semantics are deferred.

## Migration Plan

1. M1: Land disabled-by-default settings, `.qmt/knowledge.toml`, discovery, Markdown/memory adapters, the disposable index with metadata fast path, effective-checkout binding, and `repo_knowledge_*` tools. Gate on clone/rebuild, confinement, privacy, worktree, edit/deletion, malformed-source, and legacy-tool fixtures.
2. M2: Add symbol-level navigation maps and exports.
3. M3: Add per-document evidence, checks, scaffolding, and acknowledgement with the maintenance example.
4. M4: Add independently opt-in orientation and post-index adapters, shared budgets, stable policy, and compaction/resume handling. Default profiles remain unchanged until their gates pass.
5. M5: Add reviewed pilot documents and fixture corpora, run deterministic evaluation, and document optional model-backed comparisons.
6. Roll back by disabling repository knowledge and removing only its cache if desired. Committed Markdown and `.qmt/` files remain readable without QueryMT; private session data, legacy imports, and `.agents/` are unchanged.
