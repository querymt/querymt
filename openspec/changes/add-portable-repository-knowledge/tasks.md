# Tasks

## 1. Portable document contract and safe discovery

- [ ] 1.1 Reconcile the initial settings/builder plumbing with disabled-by-default host opt-in, explicit root, source/output limits, and independently disabled delivery controls; verify configuration round trips and disabled standalone/quorum configurations cause no repository reads, writes, or prompt changes before marking this task complete.
- [ ] 1.2 Implement `.agents/knowledge.toml` v1 document/code patterns and explicit `[[bundles]]` entries with repository-relative root and `format = "okf"`; verify excludes, ignore policy, missing/unsupported manifests, duplicate selection, overlapping bundle roots, conflicting adapters, and optional orientation entrypoints with fixtures.
- [ ] 1.3 Implement effective-checkout root resolution and cache identity; verify repository-root/crate-cwd equivalence, nested repositories, explicit non-Git roots, linked worktrees, and denied/unmounted roots.
- [ ] 1.4 Implement confined regular-file discovery with file/count/byte budgets; verify traversal, external symlinks, ignored secrets, selected hidden documentation, and incomplete-coverage diagnostics.
- [ ] 1.5 Add isolated plain-Markdown, legacy-memory, and OKF v0.2 adapters with a proper bounded YAML reader for non-legacy metadata; verify nested sources/generated/verified, bare verified mappings, resource-limit rejection, unknown types/keys, unknown OKF version diagnostics, and unchanged legacy parsing/enabled semantics.
- [ ] 1.6 Normalize explicit IDs and bundle-path identities, sections, original coordinates, and metadata; verify moves, duplicate IDs/headings, reserved filenames only inside bundles, mixed inputs, unknown-field preservation, and source attribution keyed by stable IDs.
- [ ] 1.7 Implement bundle-root/document-relative reference resolution distinct from filesystem paths; verify authorized references to repository code outside bundles, escape rejection, inert URLs/scope descriptors, and that links never select extra inputs or grant access.
- [ ] 1.8 Document the manifest, adapters, identity/path rules, parser limits, and concept-first authoring conventions; validate examples and OKF v0.2 compatibility fixtures without requiring frontmatter conversion of ordinary Markdown.

## 2. Checkout-local projection and complete pull retrieval

- [ ] 2.1 Implement the separate repository service/store and versioned SQLite/FTS schema with transactional generations; verify create/reopen, corruption/incompatible-cache rebuild, portable logical keys, and no session DB migrations.
- [ ] 2.2 Implement hash-based selected-input reconciliation and unchanged-parse reuse; verify external edits without watcher events, manifest changes, additions/deletions, disabled/malformed sources, renames, and branch switches never silently serve obsolete bodies.
- [ ] 2.3 Implement serialized refresh, process-safe transactions/busy handling, selected-body revalidation, and bounded unavailable/retry behavior; verify concurrent readers/processes and injected failures never expose partial generations.
- [ ] 2.4 Bind an optional repository service through standalone/quorum materialization, execution context, and tool context using the effective session checkout; verify same-checkout reuse, different-checkout isolation, resume, and no builder-time discovery.
- [ ] 2.5 Implement documentation retrieval and path-level navigation with deterministic lexical ranking, safe question handling, filters, deduplication, and exact-path priority; verify conceptual/path fixtures, punctuation, empty input, and stable ties before adding symbol extraction.
- [ ] 2.6 Add repository branches to knowledge_query/list/stats and repository-only knowledge_get; verify unchanged legacy defaults/required question, scope conflicts, explicit legacy-only parameter rejection, current section bodies, path-only catalogs, duplicate/stale selectors, pagination, and generation-change diagnostics.
- [ ] 2.7 Implement bounded authored-index consumption and read-only catalog synthesis from selected titles/descriptions; verify derived labels, source citations, missing descriptions/indexes, continuation, preserved authored listings, and no automatic fallback enablement or repository writes.
- [ ] 2.8 Format results with original citations, stable source-footnote attribution, separate index/evidence/review/lifecycle/expiry signals, and default/max count/byte budgets; verify UTF-8-safe truncation, attribution continuation, inert computation metadata, and that human prefixes/stable status never certify correctness.
- [ ] 2.9 Enforce checkout authorization on every query/fetch and preserve privacy boundaries; verify unsafe paths, cross-checkout IDs, denied roots, legacy imports/consolidations, no global/private leakage, and one repository projection per source.
- [ ] 2.10 Document pull retrieval, cache recovery, metadata signals, and progressive disclosure; run a vertical-slice gate covering clone/rebuild equivalence, worktrees, source edits/removals, malformed siblings, confinement, bounded responses, and privacy before starting automatic delivery.

## 3. Deterministic source navigation and maps

- [ ] 3.1 Adapt existing AST/outline extraction to file/symbol records with qualified names, signatures, parents, ranges, digests, and syntactic imports; verify existing-language fixtures and exact identifier ranking without changing index snapshots or inferring semantic edges.
- [ ] 3.2 Extract Rust module/item documentation verbatim with source coordinates and provenance; verify line/block forms and unsupported-language path records without generated purposes.
- [ ] 3.3 Cache extraction by source/config/extractor fingerprints; verify changed-file-only reparsing, extractor invalidation, removed symbols, unsupported sources, and parse-failure diagnostics.
- [ ] 3.4 Parse optional managed/ordinary File/Purpose rows as citable navigation records and render bounded maps; verify relative paths, empty purposes, authored preservation, and no requirement for exhaustive tables or per-symbol OKF documents.
- [ ] 3.5 Add explicit generated-artifact/block export with preimage validation; verify byte-identical portable exports across checkout paths, truncation guidance, stale-plan refusal, and preservation of authored content.
- [ ] 3.6 Document source-derived versus authored knowledge and optional exports; verify generation works offline without credentials and existing source/read/edit-precondition tests remain unchanged.

## 4. Independent evidence and deterministic maintenance

- [ ] 4.1 Define portable `.agents/knowledge-evidence.json` and explicit knowledge_sources selectors separately from sources, links, and knowledge_paths applicability; verify deterministic serialization, whole-file/unambiguous-symbol hashes, unsafe selector rejection, and no implicit hashing of URLs/scope descriptors.
- [ ] 4.2 Evaluate unverified/unchanged/changed/missing evidence independently of generated/verified/status/stale_after; verify note/dependency edits, missing/ambiguous selectors, fresh clones, and that indexing or review declarations never acknowledge bytes.
- [ ] 4.3 Implement read-only checks for malformed/conflicting inputs, broken bundle/local references, unresolved source-footnote IDs, evidence drift, lifecycle/expiry, file-table orphans, entrypoints/path associations, and budgets; verify stable relative JSON diagnostics, nonzero failing-check status, optional-index/unknown-field tolerance, curated-mode acceptance, and no remote requests/writes.
- [ ] 4.4 Implement optional scaffold preview/apply restricted to new non-root directory documents or managed inventory blocks; verify sorted empty-purpose additions, idempotence, existing-purpose/orphan preservation, unmarked-file refusal, and stale-preimage rejection.
- [ ] 4.5 Implement explicit selected-record acknowledgement preview/apply with atomic confined writes; verify note/dependency preimages invalidate stale plans, no bulk implicit acknowledgement, and metadata rewrites preserve unknown fields or refuse lossy changes.
- [ ] 4.6 Expose typed refresh/map/check/scaffold/acknowledge operations through examples/repository_knowledge.rs with JSON and explicit write options; verify offline invocation, failing-check exit codes, path policy, and no model startup or Git commits.
- [ ] 4.7 Document signal meanings, maintenance commands, managed blocks, and safe plan/apply workflows; execute examples against temporary fixture repositories and confirm normal index synthesis never writes index.md.

## 5. Optional lifecycle delivery using the same retrieval service

- [ ] 5.1 Connect workspace-change events as coalesced refresh hints; verify missed events, shell/external writes, additions, and branch changes still reconcile on explicit retrieval.
- [ ] 5.2 Add a bounded prompt-orientation selector with conservative lexical gating and explicitly configured first-task/post-compaction entrypoints; verify path-free discovery, unrelated/generic prompts yielding nothing, invalid entrypoints, authored-description/excerpt reuse, and no model calls or fallback inferred from synthesized catalogs.
- [ ] 5.3 Implement knowledge-for-path selection from canonical effective index arguments, optional purpose rows, explicit dependencies, and knowledge_paths associations; verify directory/file applicability, no fuzzy basenames, no duplicated outline signatures, distinct provenance, and current-source labels.
- [ ] 5.4 Apply lifecycle/evidence delivery eligibility; verify draft/deprecated concepts never become automatic cards, expired or missing-evidence concepts supply only cautionary pointers, changed evidence is labeled, and explicit retrieval remains available.
- [ ] 5.5 Add native user-prompt and successful built-in post-index adapters using typed hook contributions; verify tool-call/path provenance, unchanged raw outlines/error flags, result-policy ordering, foreign-tool rejection, and no recursive tool calls or double delivery through command wrappers.
- [ ] 5.6 Bound delivery time and preserve successful tool results on unavailable refresh/selector failure; verify denied/outside-corpus/failed calls attach nothing and suppression/redaction never regains restricted information.
- [ ] 5.7 Implement shared per-turn budgets (two cards, 600 estimated tokens, 4 KiB default ceiling) and stable parallel allocation; verify aggregate limits, provider protocol placement, and that omitted cards are not marked delivered.
- [ ] 5.8 Persist delivery identity/version/epoch provenance and reconstruct duplicate suppression on resume; verify actual inclusion advances state, retries reuse prepared context, and disposable diagnostics loss does not cause reinjection.
- [ ] 5.9 Re-evaluate bounded recently relevant cards after compaction using current sources; verify retained contributions are not duplicated and removed/changed documents cannot return from stale payloads.
- [ ] 5.10 Add local matched/delivered/suppressed/fetched diagnostics and one stable capability-gated retrieval policy; verify no raw prompts/shared statistics, no claims that delivery implies followed advice, unchanged system prefixes, and usable citations when query/get are excluded.
- [ ] 5.11 Document independently opt-in stages, eligibility, relevance gates, budgets, and fallback; run standalone/quorum prompt/trust fixtures showing AGENTS evidence never changes permissions/instruction precedence and composition with other hooks preserves output policy.

## 6. Controlled concept-first crates/agent pilot

- [ ] 6.1 Prepare reviewed subsystem concepts and small linked indexes plus maps for selected runtime/session/tool subtrees; retain exhaustive purpose rows only as an optional comparison arm and verify root instructions stay lean and no semantic descriptions are generated by scaffolding.
- [ ] 6.2 Freeze sanitized navigation/conceptual questions and held-out expectations at a pinned source revision before evaluation; verify no transcript, secret, absolute developer path, or personal session DB dependency.
- [ ] 6.3 Add offline deterministic retrieval/delivery replay reporting corpus/revision/index version, recall/MRR, wrong-file rate, bytes, cold/warm indexing cost, false positives/misses, duplicate cards, tokens, and latency; verify fresh-cache repeatability and positive/negative path-free/parallel/compaction cases.
- [ ] 6.4 Provide optional repeated model-backed baseline/curated/exhaustive comparisons and same-corpus pull-only/index-enriched/orientation comparisons; verify reports include model/configuration, tool/token cost, first relevant inspection, correctness, and authoring/maintenance effort without requiring model access in default tests.
- [ ] 6.5 Freeze relevance tuning before held-out replay and report outcomes/limitations; verify corpus and delivery effects are separated, unrun trials are labeled, and inconclusive results never mandate exhaustive repository-wide coverage.

## 7. Cross-cutting acceptance and opt-in rollout

- [ ] 7.1 Run the integration matrix for mixed formats, nested YAML limits, bundle references, unknown metadata, source attribution, clone/cache loss, source/manifest changes, linked worktrees, concurrent refresh, private isolation, delivery deduplication/compaction, and result-policy preservation; verify contracts across all four delta specs.
- [ ] 7.2 Run supported formatting/lint and relevant knowledge, dotagents, source/index/symbol, configuration, tools, hook/context, and session suites; record commands/results and verify no executor/attester execution, qndx dependency, or legacy behavior changes.
- [ ] 7.3 After stage-specific correctness gates pass, publish an opt-in coder example demonstrating browse/query/get, concept/source citations, maintenance, and optional delivery; validate documented configuration and rollback by disabling the feature and discarding only its cache.
