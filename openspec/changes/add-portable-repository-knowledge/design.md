# Design

## Context

See proposal.md for motivation and scope. This is a cross-cutting configuration, storage, retrieval, and workspace-lifecycle change.

Observed integration points:

- `crates/agent/src/knowledge/mod.rs:277` defines session-oriented ingest/query/consolidation and protocol reconciliation. `crates/agent/src/knowledge/sqlite.rs:990` implements lexical retrieval with metadata boosts, not embeddings.
- `crates/agent/src/tools/builtins/knowledge_query.rs:104` selects the session scope by default; the returned entries omit their raw body. `knowledge_list.rs:91` lists unconsolidated entries, not a document corpus.
- `crates/agent/src/dotagents/mod.rs:92` derives protocol scope from a canonical workspace path. Protocol imports support stable source keys, fingerprints, and inactive records. Their summaries can be only the title (`dotagents/memory.rs:144`).
- `crates/agent/src/index/symbol_index/types.rs:111` already provides names, signatures, ranges, parent/child structure, and digests. Outline and symbol extractors cover Rust and other languages; reuse them rather than maintaining another parser.
- `crates/agent/src/index/workspace_actor.rs:277` processes file changes. Existing watchers are useful invalidation hints, not evidence that no external write was missed.
- `crates/agent/src/tools/context.rs:39` and `agent/execution_context.rs:141` carry tool capabilities and knowledge access. Session materialization and execution context must supply the effective checkout, not a fixed builder cwd.
- `crates/agent/src/hooks/engine.rs:974` accepts post-tool `additional_context` with handler and tool-call provenance. `agent/execution/tool_calls.rs:1119` persists contributions after all tool-result blocks in the combined message, preserving call/result adjacency. User-prompt context contributions and the post-compaction lifecycle are also available.
- The single-coder profile does not expose knowledge tools (`examples/confs/single_coder.toml:21`). Prompt policy alone cannot fix an unavailable tool or incorrect scope.
- Pi's `packages/kb/src/dox.ts:1` explicitly separates deterministic inventory/audit from LLM-authored purposes. Its extension adds retrieval lanes, body fetch, reindexing, and freshness diagnostics; these ideas do not require copying its mandatory lookup-before-every-read rule.

Specification compatibility:

- Preserve `openspec/specs/index-tool/spec.md`, including byte-identical existing outline formatting.
- `add-dotagents-protocol-support` is complete but unarchived. Its protocol import and prompt merge contracts remain unchanged. Repository knowledge is an additional read projection of selected workspace files, not a replacement global/workspace protocol resolver.
- `add-git-worktree-isolation` is still a proposal. Accept an explicit checkout root from future workspace bindings, but support ordinary repositories and linked worktrees without depending on its new APIs.

## Goals / Non-Goals

**Goals:**
- Reduce model-side orientation work, not merely filesystem search latency.
- Let a fresh clone reconstruct useful knowledge without another developer's SQLite database or session history.
- Separate source-derived facts, authored explanations, and private memories in both storage and output.
- Support incremental reuse with explicit freshness, bounded retrieval, and reproducible maintenance.

**Non-Goals:**
- qndx, vectors/embeddings, a graph database, whole-program semantic resolution, or inferred call/dependency graphs.
- Automatic LLM summaries, automatic knowledge publication from sessions, background commits, or automatic edits to authored notes.
- New recursive AGENTS instruction enforcement or blocking all source reads until a KB query succeeds.
- A production-wide exhaustive documentation mandate before evaluation.
- A generic trigger language, command/error blocking guards, automatic trigger extraction from prose, classifier-based relevance gates, web/session memory consolidation, or retrospective lesson mining.

## Decisions

### 1. Git-tracked source manifest and Markdown are authoritative

Add host-controlled `repository_knowledge` settings to standalone/quorum configuration and builders. The feature is disabled by default. Host settings enable it, optionally provide a root, constrain allowed source roots, and set size/output budgets. A repository file cannot enable the feature, expand host permissions, or grant tools.

Read `<root>/.agents/knowledge.toml` when enabled. Proposed v1 shape:

```toml
version = 1

[documents]
include = [".agents/memories/*.md", "docs/architecture/**/*.md", "crates/agent/**/AGENTS.md"]
exclude = ["openspec/changes/archive/**"]

[code]
include = ["crates/agent/src/**/*.rs"]
exclude = []
```

Paths/globs are relative to the selected root; excludes win. Missing manifest means diagnostic `not_configured`, not an implicit repository-wide crawl. An explicit maintenance preview can propose a manifest but cannot create it without an apply action. Unknown manifest versions are rejected. Host limits cap files, bytes per file, and total indexed bytes; skipped sources are disclosed as incomplete coverage rather than an exhaustive answer.

Workspace memories retain the current frontmatter parser and enabled/id/title/tags semantics. Other Markdown needs no special frontmatter. Explicit IDs are stable across moves; otherwise identity derives from relative path. Duplicate document IDs disable the conflicting documents with diagnostics rather than selecting one nondeterministically. Optional `knowledge_sources` frontmatter is a list of relative paths with optional qualified-symbol selectors for evidence checking. Documentation remains useful without evidence metadata, but is labeled unverified.

All reads validate canonical confinement and reject traversal, external symlinks, and non-regular sources. Git-ignored content is excluded by default, even when a broad glob matches; an explicit host allow rule is required to include it. Selected hidden documentation such as `.agents/memories` is allowed. There is no ambient scan of home directories, global protocol memories, session stores, or unrelated worktrees.

Alternative: exporting session SQLite mixes private data with derived artifacts and makes review/versioning poor. A separate proprietary document format duplicates Markdown and current protocol parsing.

### 2. Checkout-local derived store, within the existing knowledge module

Add a repository-knowledge service and repository-store interface under `src/knowledge/`. Keep the legacy `KnowledgeStore` trait, retention, consolidation, and protocol-owned tables unchanged. Reuse SQLite/FTS patterns and text utilities where appropriate, but give repository documents their own schema and lifecycle.

Use a rebuildable database at the host's cache directory, default `~/.qmt/cache/knowledge/<checkout-key>/index.sqlite`. It is never copied into Git or embedded into `sessions.db`. A checkout key incorporates the canonical checkout root, selected knowledge root, and index/config version. Shared Git common-dir identity alone is insufficient: linked worktrees must not overwrite each other's active corpus. Public citations and portable IDs never contain this machine-local key.

Suggested tables: index generations/config metadata; sources (relative path, kind, stable document ID, content hash); chunks (heading/symbol identity, ranges, body, tags); code records; evidence dependencies; FTS tables. Store source-owned authored documents separately from generated navigation records. Stable chunk keys use document ID plus heading breadcrumb and occurrence, or file path plus qualified symbol, kind, and occurrence. Heading/symbol renames can invalidate keys; `knowledge_get` must report that rather than return a different section.

Workspace `.agents/memories` can still be imported by the existing protocol system. The repository projection reads those files directly and deduplicates overlapping manifest patterns. Repository queries never union the protocol/session tables, so coexistence does not produce duplicate repository hits or leak global overrides. Deleting a repository document affects only the derived repository index. Repository chunks are not consolidation inputs; legacy consolidations remain private and cannot resurrect removed repository claims through this collection.

Alternative: changing default scopes or using `deactivate_protocol_sources` for all documents risks silently altering legacy imports and deleting records owned by another loader.

### 3. Root selection and refresh are explicit

Root precedence: host-provided effective checkout/knowledge root; otherwise discover the Git worktree root containing the session cwd; otherwise require an explicit root for non-Git workspaces. The cwd must lie within the selected root. Do not use the nearest Cargo manifest as repository identity: starting at `crates/agent` should find the same root manifest as starting at repository root. Nested Git repositories select their own worktree unless the host explicitly supplies a containing authorized root.

Bind the repository service to each effective session workspace and share it across sessions on the same checkout/configuration. No repository access exists when cwd/root is unavailable, on a remote filesystem not mounted on the executor, or when policy denies the selected root.

Refresh on first repository retrieval, explicit refresh, and subsequent retrievals after changes. Initial implementation deterministically enumerates configured inputs and compares content hashes before serving a query; it parses only changed files and reuses unchanged chunks. Watcher events coalesce work and accelerate invalidation, but are not the only correctness mechanism. This prioritizes correctness over eliminating all disk reads. Profile hashing cost before introducing weaker metadata shortcuts.

Reconcile additions, modifications, disabled/deleted sources, manifest changes, and checked-out branch contents. Changes apply transactionally to an index generation. Serialize refresh per checkout and use SQLite transactions/busy handling for multiple processes. Readers receive a complete generation, never partially rebuilt FTS state. Recheck selected source hashes before emitting bodies/coordinates; bounded retries handle concurrent edits, then return an explicit updating/unavailable diagnostic. This is not a filesystem snapshot against arbitrary external writers.

If a source becomes invalid, do not present its previous body as current. Exclude that source from live results and report a source-specific diagnostic while valid siblings remain usable. Global refresh failure or cache corruption produces an unavailable diagnostic and normal-code-search guidance; do not silently answer from stale cache. The cache can be rebuilt without touching authoritative files or private knowledge.

### 4. Deterministic structure supplements authored meaning

Reuse `SymbolIndex` and the shared outline extraction pipeline. Persist file inventories and compact records containing path, language, kind, qualified name, signature, parent, ranges, and digest. Existing source documentation can be copied verbatim with source coordinates; first version supports Rust module/item doc comments, while other languages retain their existing structural extraction coverage.

Represent declared imports as syntax, not resolved semantic dependencies. Unsupported/unparseable files remain visible as path-only records with coverage diagnostics. Generated facts never claim a semantic purpose from a filename. Source and extractor/schema fingerprints invalidate cached records. Rendering and serialized map export use stable relative-path/symbol ordering without timestamps or machine-local paths.

Expose directory/file maps on demand through repository navigation retrieval and maintenance `map`. Generated maps remain local by default. An explicit export can produce a portable Markdown map; regenerating it replaces only a designated generated artifact/block, never prose in an arbitrary AGENTS file. Existing `index`, `get_symbol`, and edit precondition behavior stays unchanged.

Alternative: exhaustive LLM-written descriptions are neither deterministic nor necessary for mechanical symbol navigation. AST structure alone, however, cannot explain architectural rationale; retain reviewed prose for that.

### 5. Extend tools additively, not by changing legacy defaults

Add `collection: "repository"` to `knowledge_query`, `knowledge_list`, and `knowledge_stats`. Omission preserves existing behavior, including raw `scope` authorization and session defaults. `collection: "repository"` with a raw `scope` is rejected as ambiguous. Repository authorization validates the effective checkout on every operation; callers cannot select another checkout by supplying an ID or path.

Repository query parameters retain the existing required `question` field and add `lane` (`navigation`, `documentation`, `all`; default `all`), an optional relative path filter, and `limit`. Explicit legacy-only `retrieval_mode` or `include_consolidations` arguments are rejected for repository mode with an explanation; their omission does not apply legacy defaults to the repository collection. Navigation ranks exact path/qualified-name/name matches and generated facts/file-table rows. Documentation ranks authored heading/body matches using FTS5 BM25 and deterministic tie-breaking. `all` merges deduplicated results under a shared budget. Treat query text as user text, not raw FTS syntax; empty/invalid input has a clear error. No model call or external service is needed.

Results contain a repository-local retrievable ID, kind/provenance, relative source path, original line range, section/symbol selector, bounded excerpt, and freshness/evidence state. Source-derived facts and authored descriptions remain visibly distinct. Default result limit is 10, hard maximum 50. Default serialized response budget is 16 KiB, host-capped at 64 KiB; report truncation and how to narrow/fetch rather than silently omit it.

Add repository-only `knowledge_get` accepting either an ID or a relative path plus optional section selector, with bounded pagination. A path-only multi-section request returns a section catalog and preamble, not an arbitrary first section disguised as the whole document. Responses expose continuation and remaining-section information. Fetches revalidate checkout authorization, current source identity, and freshness.

Repository `knowledge_list` is a paginated document/navigation catalog, not the legacy unconsolidated-entry view. Repository `knowledge_stats` reports coverage, active source/chunk counts, index version/generation, and diagnostics/freshness counts. Existing ingest/consolidate operations remain local-memory operations and cannot mutate repository files.

### 6. Two delivery stages backed by one repository service

A post-index hook alone cannot solve initial orientation: an unfamiliar agent does not yet know which file to index. Keep explicit query/get available and implement both automatic stages below. Host `repository_knowledge.delivery` settings independently enable `orientation` and `index_context`; both default off. Repository metadata can describe associations but cannot enable handlers, grant access, or supply executable hook commands.

```text
User task without a known source path
  --> orientation trigger --> ranked subsystem/source pointers
  --> agent chooses index(path)
  --> structural outline + attributed knowledge-for-path digest
  --> focused knowledge_get/source read --> edit with current-source checks
```

#### 6.1. Prompt-triggered orientation before file selection

At accepted user-prompt submission, before the first model request for that turn, perform bounded local lexical retrieval using the user's task text. This does not depend on a path, symbol, previous tool call, or model-issued knowledge request. Search authored titles/headings/tags and source documentation with the existing repository retrieval service; links and declared source associations provide concrete candidate file/subsystem pointers. No LLM is called and no triggers are inferred from arbitrary lesson prose.

Use an explicit conservative relevance gate: an exact path/qualified-identifier match, or at least two distinct non-stopword query terms matching indexed titles/headings/tags/document text. Rank gated results with the existing deterministic lexical ranking. This is an explainable initial heuristic, not a calibrated confidence score; common-term false positives must be measured by replay. Query length and term count are bounded, and unrelated/no-match prompts produce no search-derived cards.

For the first accepted task in a session, and once after compaction if needed, an optional manifest `[orientation]` `entrypoints` list can supply a small reviewed repository map when no gated result exists. Values are stable selected-document IDs or relative document paths with section selectors, not executable rules or an inferred source-file choice. Missing configuration means no fallback card; invalid entrypoints produce diagnostics rather than a repository-wide dump. This separates 'where can I start?' from pretending to know the right implementation file. Existing documents need not be rewritten just to add prompt triggers.

Append selected cards as clearly attributed user-prompt context through the existing contribution pipeline, not by replacing the system prompt. A card contains an authored short description or verbatim bounded excerpt/heading, current relative citations, source pointers where available, and evidence state. It identifies candidate entry points, not a mandatory edit target.

#### 6.2. Successful index calls receive knowledge-for-path context

At the post-tool stage for the built-in `index`, resolve the actual successful indexed file with the same root/path semantics as the tool, including effective inputs after any rewrites. Do not trust tool name alone for an MCP tool with the same name. Confirm the canonical file is in the authorized selected corpus. Match direct File/Purpose rows, `knowledge_sources` dependencies, and explicitly declared directory associations. Add optional `knowledge_paths` frontmatter containing repository-relative globs for advisory applicability; unlike evidence dependencies, these globs do not certify freshness or truth. No fuzzy basename matching and no searching all result text for incidental path mentions.

Return a compact digest through `additional_context`/typed hook contributions tied to the tool-call ID and indexed relative path. Preserve the original structural outline byte-for-byte and do not patch `updated_output` or its error flag. Contributions follow the combined result blocks for parallel calls, so each names its target file. Prefer authored purpose, relevant non-obvious invariants, and architecture/test pointers; do not duplicate the outline's signatures. A digest is deterministic selection of existing content, not generated semantic prose. Label changed/unverified evidence explicitly; missing evidence yields only a cautionary pointer, not an invariant presented as current.

Skip failed/denied index calls, out-of-corpus files, absent knowledge, and exhausted delivery budgets. Enrichment errors/timeouts leave the successful index result intact and emit a rate-limited host diagnostic. The provider invokes the repository service directly, not model-visible knowledge tools, avoiding recursive hooks and extra model round trips.

#### 6.3. Native lifecycle adapters reuse hook contribution semantics

Implement the two selectors as shared repository-service operations and attach in-process adapters at user-prompt and post-tool contribution stages. This is new compiled integration; the current configurable hook engine supports command/MCP handlers, not an existing native callback registration API. Reuse the typed provenance and message assembly path without introducing a general new hook language. The maintenance example can offer a hook-JSON adapter for trusted command-hook deployments using the same operations, but the built-in profile uses the in-process provider to avoid subprocess/index-open overhead. Enable only one adapter per delivery stage; it must not deliver twice if a command wrapper is configured instead.

The index handler is observational: it neither changes permission decisions nor alters execution facts. Run it only after successful execution and applicable output-policy processing; a suppressed/redacted result must not regain restricted information through enrichment. Bound delivery time; unready refresh or selector failure skips automatic context rather than delaying the turn indefinitely or serving known obsolete data. Ordinary explicit retrieval continues to report its normal unavailable diagnostics.

#### 6.4. Shared budgets, duplicate suppression, and compaction

Both stages share a per-turn budget, initially at most two cards and 600 estimated tokens, with a 4 KiB serialized hard ceiling including labels/citations. Host settings can lower these defaults; increasing them is an explicit policy choice subject to a fixed implementation maximum. Automatic cards are intentionally much smaller than explicit retrieval responses. A card that cannot fit is omitted, never marked delivered. Allocate parallel index-call cards in stable tool-call order when assembling the combined results, rather than by completion timing.

Track delivered card ID, card/source/evidence version, session, checkout, and compaction epoch. Deduplicate across orientation and index channels and across repeated tool calls while the same version is represented in effective context. Update delivery state only when the contribution is actually included in the committed turn/request, not merely selected. Provider retries reuse the prepared context without rerunning selection. Resume reconstructs delivery state from contribution provenance and the effective compaction boundary; a discarded local diagnostics cache must not cause an injection storm.

On compaction, re-evaluate a bounded set of recently relevant cards using the active task and recently targeted indexed paths. Revalidate sources/evidence and attach retained cards through post-compaction context or the next request. Do not replay every historical card; dropped/changed documents cannot be resurrected from stale text. A fresh epoch allows needed redelivery under the same budgets. Where available, avoid duplicating cards already represented as retained typed contributions. No LLM-generated compaction summary is trusted as proof that a precise card survives.

Record local matched/delivered/suppressed counts, reasons, added tokens, and latency without writing statistics or raw prompts into shared documents. 'Delivered' and 'fetched' are observable events; neither implies 'followed'.

#### 6.5. Stable policy and trust boundary

When query/get tools are allowed, attach one compact policy explaining retrieval lanes, advisory cards, relevant-section fetch, current-source verification, and normal-search fallback. Do not mandate a lookup before targeted reads. Automatic delivery is separately host-authorized and does not implicitly enable tools; when get is unavailable, cards use navigable relative citations without instructing an unavailable tool call. Excluded tools remain excluded and diagnostics describe partial capability.

Keep the system policy stable across turns; variable cards travel in attributed contextual contributions. Test actual provider message order and prefix stability instead of assuming a universal cache-hit guarantee. Directory AGENTS content remains repository evidence, not higher-priority instructions. Never inject the entire corpus, private memory, arbitrary repository commands, or repeated policy copies.

Alternatives rejected: only post-index enrichment misses the unknown-file problem; only a 'search first' prompt relies on voluntary retrieval; a broad learned/regex trigger engine adds false positives and scope before the two concrete stages are evaluated.

### 7. Separate index freshness from evidence freshness

Index freshness means indexed bytes match current documents/source. Evidence freshness means the source supporting an authored statement matches the last explicitly acknowledged evidence. Neither proves the statement is true.

Support a portable, Git-trackable `.agents/knowledge-evidence.json` sidecar. Entries identify document/section or file-table row, the reviewed text hash, and explicit dependency paths/selectors plus versioned SHA-256 hashes. Dependencies come from declared `knowledge_sources` or the file path represented by a file-table row; use whole-file hashes first and symbol hashes only for unambiguous supported selectors. Missing/ambiguous selectors are diagnostics, not a successful match. Ordinary relative Markdown links are checked separately for broken references and do not automatically imply an evidence dependency.

Evidence states are `unverified`, `unchanged`, `changed`, and `missing`. An edited note with no corresponding acknowledgement becomes unverified; changed dependency bytes are changed, not automatically false. A vanished dependency is missing. No confident rename is inferred from a similar filename; report candidates only as suggestions. Acknowledgement is an explicit, previewable maintenance write bound to the exact note and dependency hashes, not a side effect of indexing, fetching, or generating maps. The sidecar contains no absolute paths, session IDs, timestamps required for equality, or private content.

### 8. Deterministic maintenance API and minimal executable surface

Expose typed service operations and an executable `crates/agent/examples/repository_knowledge.rs` wrapper, usable without a model/provider. This avoids adding an agent dependency and a new command family to the separate provider-oriented CLI during the first iteration.

Example invocation: `cargo run -p querymt-agent --example repository_knowledge -- --root . check --json`.

Operations:

- `refresh`: reconcile/rebuild the local index, report coverage and failures.
- `map`: render bounded file/directory structure; explicit output option for generated artifacts.
- `check`: read-only diagnostics for source coverage, malformed/duplicate documents, broken local references, file-table orphans/missing rows, evidence drift, invalid orientation entrypoints or knowledge-path associations, and configured size budgets. Missing per-file rows are only findings in exhaustive-coverage mode. JSON output has stable codes/relative paths, and a failing check returns nonzero.
- `scaffold`: preview sorted directory file-table additions; `--apply` writes only newly created files or explicitly marked managed inventory blocks. Existing AGENTS files without an unambiguous managed block are left untouched with a diagnostic. Never populate a purpose cell, remove/overwrite prose, prune orphan rows, or create a root-wide file table. Non-source root files can be reported without generating root inventory.
- `acknowledge`: preview evidence records for explicitly selected documents/rows; `--apply` records reviewed hashes only after verifying preview preimages. It does not assert correctness, generate descriptions, or blanket-acknowledge all drift.

Apply operations fail on changed preimages and write atomically within the authorized root. There is no autonomous maintenance writer tool and no implicit Git commit. Existing file-edit tools remain the explicit route for authored descriptions.

### 9. Compare documentation strategies before adopting one

Build a small opt-in pilot under `crates/agent`, covering selected runtime, session, and tool subtrees. Do not create AGENTS files across all 35 directories merely to demonstrate the feature.

Evaluation arms use the same source revision and tool budgets:

- Baseline: current source-navigation workflow.
- A: a small reviewed subsystem corpus, existing docs, and deterministic maps.
- B: the same corpus/maps plus exhaustive per-file purpose rows in the selected subtrees, with inventory scaffolding generated deterministically and descriptions reviewed before use.

For each fixed corpus, compare delivery modes independently: pull-only; pull plus index enrichment; and pull plus index enrichment plus orientation triggers. Include tasks with no file names or symbol identifiers, where the first relevant path must be discovered. Replay unrelated prompts, generic terms, external paths, failed index calls, parallel calls, and compaction/resume to measure false-positive rate, missed cues, duplicate delivery, added tokens, and latency. Record selected/delivered/fetched separately and do not infer that a suggestion was followed merely from a fetch. Freeze relevance-gate tuning on development examples before held-out evaluation. Dossier claims from Pi are motivation, not verified performance targets for QueryMT.

Include both file/symbol-location and conceptual questions. Freeze corpus authoring before held-out queries; historical session mining is optional, local-only, and requires sanitization before any examples are committed. Do not commit session transcripts or depend on a developer-specific database path. Deterministic retrieval fixtures run without a model; an opt-in end-to-end runner records model/version, repeated trials, tool/output tokens, latency, task correctness, and documentation authoring/update effort.

Mandatory correctness gates: no cross-session/private leakage; identical logical results after clone/rebuild; fresh edits/deletions/branch switches; deterministic maps; bounded output; unchanged legacy tools. Record retrieval recall/MRR and wrong-file rates separately for navigation and documentation. Compare task completion and maintenance effort, not just fewer calls. Report results and limitations; the pilot does not enable exhaustive coverage globally or require a predetermined performance win.

## Risks / Trade-offs

- Hashing all selected inputs costs I/O -> bound the corpus, reuse parsed records, batch filesystem work, and measure cold/warm refresh independently from LLM token savings.
- Source structure is not program semantics -> label generated syntax honestly; do not infer ownership, call edges, or purpose.
- Notes can be wrong even with unchanged evidence -> retain citations and source verification; hashes report drift, not truth.
- Duplicate projections of workspace memories -> separate collection boundaries and deduplicate repository discovery; preserve legacy semantics rather than silently migrate data.
- Repository-controlled instructions or secrets -> host opt-in, source confinement, explicit selection, ignored-file policy, and no elevation of retrieved content to system authority. Explicitly selected files can still contain secrets; document that host/user review is required.
- Heading moves and duplicate names -> stable selectors include occurrence; stale IDs fail explicitly and return a current catalog rather than an unrelated body.
- Multi-process refresh or external edits -> transactional generations, per-checkout serialization, preimage checks, bounded retry, and explicit unavailable state.
- Documentation maintenance can cost more than it saves -> keep generated scaffolds optional and make the pilot compare maintenance as well as retrieval.
- Prompt retrieval can deliver irrelevant context -> conservative lexical gating, optional reviewed entrypoints, explicit suppression, small shared budgets, and negative-case replay before enabling it in a profile.
- Automatic enrichment can undo output policy or duplicate context -> only authorize selected sources, respect suppression/redaction, deduplicate across channels, and retain typed provenance across retries/resume.

## Migration Plan

1. Land disabled-by-default settings, repository service/store, and fixtures without changing session DB schemas or protocol imports.
2. Add deterministic extraction/refresh and maintenance API/example; exercise clone, rebuild, worktree, corruption, and concurrency cases.
3. Add explicit repository tool modes and knowledge_get, then the independently opt-in orientation trigger and post-index contribution adapters, shared delivery controls, stable policy, and compaction/resume handling. Keep all default profiles unchanged until correctness gates pass.
4. Add reviewed pilot documents and fixture corpora, run deterministic evaluation, and document how to execute optional model-backed comparisons.
5. Roll back by disabling repository knowledge and removing only its cache if desired. Committed Markdown remains ordinary documentation; private session data and legacy imports are unchanged. Do not delete authored documents automatically during rollback.
