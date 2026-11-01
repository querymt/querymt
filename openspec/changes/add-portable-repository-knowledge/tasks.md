# Tasks

## 1. M1a: Portable document contract and safe discovery

- [x] 1.1 Add disabled-by-default `repository_knowledge` host settings and builder plumbing (explicit root, source/output limits, forced-hash option, independently disabled delivery controls); verify configuration round trips and that disabled standalone/quorum configurations cause no repository reads, writes, or prompt changes.
- [x] 1.2 Implement `.qmt/knowledge.toml` v1 (`[documents]`, `[code]`, optional `[orientation] entrypoints`); verify excludes, ignore policy, missing manifest, unsupported version, unknown top-level table rejection, duplicate selection, and that `.agents/knowledge.toml` is never read.
- [x] 1.3 Implement effective-checkout root resolution and cache identity under the shared QueryMT config directory; verify repository-root/crate-cwd equivalence, nested repositories, explicit non-Git roots, linked worktrees, and denied/unmounted roots.
- [x] 1.4 Implement confined regular-file discovery with file/count/byte budgets; verify traversal, external symlinks, ignored secrets, selected hidden `.agents/memories`, and incomplete-coverage diagnostics.
- [x] 1.5 Add plain-Markdown and legacy-memory adapters using the existing dotagents frontmatter conventions; verify no-frontmatter documents, unknown-key retention, malformed frontmatter isolation, recognized optional keys (`id`, `title`, `description`, `tags`, `status`, `knowledge_sources`, `knowledge_paths`), and unchanged legacy parsing/enabled semantics.
- [x] 1.6 Normalize explicit and path-derived identities, sections, and original coordinates; verify moves, duplicate IDs/headings, and mixed inputs.
- [x] 1.7 Implement document-relative reference resolution with confinement; verify escape rejection, inert URLs, and that links never select extra inputs or grant access.

## 2. M1b: Checkout-local projection and pull retrieval

- [x] 2.1 Implement the separate repository service/store and versioned SQLite/FTS schema with transactional generations; verify create/reopen, corruption/incompatible-cache rebuild, portable logical keys, and no session DB migrations.
- [x] 2.2 Implement reconciliation with the metadata fast path (size, mtime, file identity, racy-timestamp rule) and hash-on-mismatch reparsing; verify warm queries read no unchanged content, same-size edits in the racy window are detected, mtime-only changes reuse records, forced hashing, external edits without watcher events, manifest changes, additions/deletions, disabled/malformed sources, and branch switches.
- [x] 2.3 Implement serialized refresh, process-safe transactions/busy handling, pre-emission source revalidation, and bounded unavailable/retry behavior; verify concurrent readers/processes and injected failures never expose partial generations.
- [x] 2.4 Bind an optional repository service through standalone/quorum materialization, execution context, and tool context using the effective session checkout; verify same-checkout reuse, different-checkout isolation, resume, and no builder-time discovery.
- [x] 2.5 Implement documentation retrieval and path-level navigation records with deterministic lexical ranking, safe question handling, filters, deduplication, and exact-path priority; verify conceptual/path fixtures, punctuation, empty input, and stable ties.
- [x] 2.6 Add `repo_knowledge_query`, `repo_knowledge_get`, `repo_knowledge_list`, and `repo_knowledge_stats`; verify existing knowledge tool schemas and behavior are unchanged, current section bodies, path-only catalogs, duplicate/stale selectors, pagination, and generation-change diagnostics.
- [x] 2.7 Implement bounded authored-listing consumption and read-only directory catalog synthesis; verify derived labels, source citations, missing descriptions, continuation, preserved authored listings, and no fallback enablement or repository writes.
- [x] 2.8 Format results with citations, separate index/evidence/lifecycle fields, and default/max count/byte budgets; verify UTF-8-safe truncation and that lifecycle status never clears evidence drift.
- [x] 2.9 Enforce checkout authorization on every query/fetch and privacy boundaries; verify unsafe paths, cross-checkout IDs, denied roots, legacy imports/consolidations, no global/private leakage, and one repository projection per source.
- [x] 2.10 Document the manifest location rationale (`.qmt/` vs `.agents/`), frontmatter keys, pull retrieval, and cache recovery; run the M1 gate: clone/rebuild equivalence, worktrees, edits/removals, malformed siblings, confinement, bounded responses, privacy, no writes under `.agents/`, and unchanged legacy tools.

## 3. M2: Deterministic source navigation and maps

- [x] 3.1 Adapt existing AST/outline extraction to file/symbol records with qualified names, signatures, parents, ranges, digests, and syntactic imports; verify existing-language fixtures and exact identifier ranking without changing index snapshots or inferring semantic edges.
- [x] 3.2 Extract Rust module/item documentation verbatim with source coordinates and provenance; verify line/block forms and unsupported-language path records without generated purposes.
- [x] 3.3 Cache extraction by source/config/extractor fingerprints; verify changed-file-only reparsing, extractor invalidation, removed symbols, unsupported sources, and parse-failure diagnostics.
- [x] 3.4 Parse optional managed/ordinary File/Purpose rows as citable navigation records and render bounded maps; verify relative paths, empty purposes, authored preservation, and no requirement for exhaustive tables or per-file documents.
- [x] 3.5 Add explicit generated-artifact/managed-block export with preimage validation; verify byte-identical portable exports across checkout paths, truncation guidance, stale-plan refusal, and preservation of authored content.
- [x] 3.6 Document source-derived versus authored knowledge and exports; verify offline generation and that existing source/read/edit-precondition tests remain unchanged.

## 4. M3: Evidence and deterministic maintenance

- [x] 4.1 Define per-document evidence files at `.qmt/knowledge/evidence/<document-path>.json` with versioned, deterministic serialization and normalized text hashing (BOM strip, LF normalization, trailing-whitespace removal); verify CRLF/LF equivalence, whole-file/unambiguous-symbol hashes, unsafe selector rejection, and that links and URLs are never implicit dependencies.
- [x] 4.2 Evaluate unverified/unchanged/changed/missing evidence independently of lifecycle status; verify note/dependency edits, missing/ambiguous selectors, fresh clones, orphaned evidence files after moves, and that indexing never acknowledges bytes.
- [x] 4.3 Implement read-only checks (malformed/conflicting inputs, broken references, evidence drift, orphaned evidence files, file-table orphans, entrypoints/`knowledge_paths`, budgets); verify stable relative JSON diagnostics, nonzero failing-check status, unknown-key and missing-listing tolerance, curated-mode acceptance, and no remote requests/writes.
- [x] 4.4 Implement optional scaffold preview/apply restricted to new non-root directory documents or `qmt:inventory` managed blocks; verify sorted empty-purpose additions, idempotence, existing-purpose/orphan preservation, unmarked or multi-block file refusal, no instructions inside blocks, and stale-preimage rejection.
- [x] 4.5 Implement explicit selected-record acknowledgement preview/apply with atomic confined writes; verify note/dependency preimages invalidate stale plans, no implicit bulk acknowledgement, and independent-branch acknowledgements merge without conflict.
- [x] 4.6 Expose typed refresh/map/check/scaffold/acknowledge operations through `examples/repository_knowledge.rs` with JSON and explicit write options; verify offline invocation, failing-check exit codes, path policy, no writes under `.agents/`, and no model startup or Git commits.
- [x] 4.7 Document evidence meanings, hash normalization, maintenance commands, managed blocks, and plan/apply workflows; execute examples against temporary fixture repositories.

## 5. M4: Optional lifecycle delivery using the same retrieval service

- [x] 5.1 Connect workspace-change events as coalesced refresh hints; verify missed events, shell/external writes, additions, and branch changes still reconcile on explicit retrieval.
- [x] 5.2 Implement relevance gate v1 exactly as specified (NFKC/lowercase, 2 KiB cap, identifier splitting, 64-token cap, versioned stopword and generic-verb lists, 2-metadata/3-body thresholds, exact path/identifier bypass); verify reproducibility, generic and non-English prompts yielding nothing, exact-path bypass, and stable ordering.
- [x] 5.3 Add the prompt-orientation selector with configured first-task/post-compaction entrypoint fallback; verify path-free discovery, invalid entrypoints, authored-description/excerpt reuse, and no model calls or fallback from synthesized catalogs.
- [x] 5.4 Implement knowledge-for-path selection from canonical effective index arguments, purpose rows, `knowledge_sources`, and `knowledge_paths`; verify directory/file applicability, no fuzzy basenames, no duplicated outline signatures, distinct provenance, and current-source labels.
- [x] 5.5 Apply lifecycle/evidence delivery eligibility; verify draft/deprecated documents never become automatic cards, missing evidence supplies only cautionary pointers, changed evidence is labeled, and explicit retrieval remains available.
- [x] 5.6 Add native user-prompt and successful built-in post-index adapters using typed hook contributions; verify tool-call/path provenance, unchanged raw outlines/error flags, result-policy ordering, foreign-tool rejection, and no recursive tool calls or double delivery through command wrappers.
- [x] 5.7 Bound delivery time and preserve successful tool results on unavailable refresh/selector failure; verify denied/outside-corpus/failed calls attach nothing and suppression/redaction never regains restricted information.
- [x] 5.8 Implement shared per-turn budgets (two cards, 600 estimated tokens, 4 KiB default ceiling) and stable parallel allocation; verify aggregate limits, provider protocol placement, and that omitted cards are not marked delivered.
- [x] 5.9 Persist delivery identity/version/epoch provenance and reconstruct duplicate suppression on resume; verify actual inclusion advances state, retries reuse prepared context, and diagnostics-cache loss does not cause reinjection.
- [x] 5.10 Re-evaluate bounded recently relevant cards after compaction using current sources; verify retained contributions are not duplicated and removed/changed documents cannot return from stale payloads.
- [x] 5.11 Add local matched/delivered/suppressed/fetched diagnostics and one stable capability-gated retrieval policy; verify no raw prompts/shared statistics, no "followed" inference, unchanged system prefixes, and usable citations when repository tools are excluded.
- [x] 5.12 Document independently opt-in stages, the gate specification, budgets, and fallback; run standalone/quorum prompt/trust fixtures showing AGENTS evidence never changes permissions/instruction precedence and composition with other hooks preserves output policy.

## 6. M5: Controlled concept-first crates/agent pilot

- [x] 6.1 Prepare reviewed subsystem documents and small linked listings plus maps for selected runtime/session/tool subtrees; retain exhaustive purpose rows only as an optional comparison arm and verify root instructions stay lean and no semantic descriptions are generated by scaffolding.
- [x] 6.2 Freeze sanitized navigation/conceptual questions and held-out expectations at a pinned source revision; verify no transcript, secret, absolute developer path, or personal session DB dependency.
- [x] 6.3 Add offline deterministic retrieval/delivery replay reporting corpus/revision/index/gate version, recall/MRR, wrong-file rate, bytes, cold/warm refresh cost, false positives/misses, duplicate cards, tokens, and latency; verify fresh-cache repeatability and positive/negative path-free, non-English, parallel, and compaction cases.
- [ ] 6.4 Provide optional repeated model-backed baseline/curated/exhaustive comparisons and same-corpus pull-only/index-enriched/orientation comparisons; verify reports include model/configuration, tool/token cost, first relevant inspection, correctness, and authoring/maintenance effort without requiring model access in default tests.
- [x] 6.5 Freeze the gate version and word lists before held-out replay and report outcomes/limitations; verify corpus and delivery effects are separated, unrun trials are labeled, and inconclusive results never mandate repository-wide exhaustive coverage.

## 7. Cross-cutting acceptance and opt-in rollout

- [x] 7.1 Run the integration matrix for mixed Markdown/memory inputs, frontmatter edge cases, references, clone/cache loss, fast-path freshness, source/manifest changes, linked worktrees, concurrent refresh, private isolation, `.agents/` non-modification, delivery deduplication/compaction, and result-policy preservation; verify contracts across all four delta specs for the milestones being shipped.
- [x] 7.2 Run supported formatting/lint and relevant knowledge, dotagents, source/index/symbol, configuration, tools, hook/context, and session suites; record commands/results and verify no qndx dependency or legacy behavior changes.
- [x] 7.3 After stage-specific gates pass, publish an opt-in coder example demonstrating browse/query/get, document/source citations, maintenance, and optional delivery; validate documented configuration and rollback by disabling the feature and discarding only its cache.
- [x] 7.4 Record deferred work (OKF bundles, nested-YAML metadata, review/expiry declarations, claim-footnote attribution) as a follow-up proposal outline; verify M1 contracts do not need to change to accommodate it.

## Verification record (7.2)

Run from the repository root on `feat/kb`; CI feature set `dashboard,oauth,remote`.

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy -p querymt-agent --all-targets --features "dashboard,oauth,remote" -- -D warnings` | clean |
| `cargo nextest run -p querymt-agent --features "dashboard,oauth,remote" --no-fail-fast` | 4181 run: 4180 passed, 1 failed, 9 skipped |
| `cargo nextest run -p querymt-agent --test repository_knowledge_matrix` (7.1 matrix) | 10/10 passed (also within the full run) |
| `openspec validate add-portable-repository-knowledge` | valid |

The one failure, `agent::handle::remote_connect::tests::connect_budget_defaults_are_bounded_and_reason_specific`, also fails on a clean `HEAD` worktree, so it predates this change and is unrelated to repository knowledge. Under plain `cargo test` two further `remote_ext` tests also failed; they fail on a clean `HEAD` too and pass under nextest's per-test process isolation.

Legacy checks: no changes to `knowledge/sqlite.rs`, `knowledge/mod.rs`, `dotagents/`, `index/`, or the legacy `knowledge_*`/`index` tools (`git status`); no `qndx` reference in `crates/agent` or `Cargo.lock`.

