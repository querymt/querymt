# Follow-up proposal outline: deferred repository-knowledge work

Not part of `add-portable-repository-knowledge`. Recorded so a later change can
pick it up without altering the M1 contracts.

## Deferred items

1. **Open Knowledge Format (OKF v0.2) bundles**: `[[bundles]]` manifest
   entries, reserved `index.md`/`log.md` roles, bundle-root `/path` references,
   a concept `type` requirement, and OKF identity mapping.
2. **Nested-YAML metadata**: a general bounded reader for metadata the
   dotagents frontmatter parser cannot express.
3. **Review/expiry declarations**: `verified`/`generated` authorship,
   `stale_after` expiry, and inert executor/attester metadata.
4. **Claim-footnote attribution**: `sources` provenance with per-claim
   footnotes.

## Why M1 contracts do not need to change

| M1 contract | Accommodation |
|---|---|
| Manifest v1 rejects unknown top-level tables | `[[bundles]]` arrives with manifest `version = 2` or an explicit opt-in, so it is never half-interpreted by a v1 reader. |
| Documents are plain Markdown; unknown frontmatter keys are retained and ignored | OKF concept files are already indexable as plain Markdown. Keys for 2-4 can be added to the recognized set without invalidating existing documents. |
| Adapter boundary separates plain Markdown from legacy memories | A bundle adapter is an additional implementation of the same interface. |
| Evidence is per-document under `.qmt/knowledge/evidence/` with a `version` field | Review/expiry and footnote data are new versioned fields or sibling files; unchanged-hash semantics stay as they are. |
| Evidence state is independent of lifecycle `status` | Authorship and expiry labels would be further separate signals, never changing evidence state. |
| Portable IDs are explicit `id:` or path-derived `path:` | OKF identity maps onto explicit IDs. |

## Open questions for that proposal

- Whether a cross-tool knowledge convention exists by then and should be accepted as another manifest location.
- Whether nested YAML justifies a real parser dependency or a restricted subset.
- Whether expiry is advisory (a label) or may affect automatic-delivery eligibility.

## Prerequisites

M1-M4 shipped and measured in the `crates/agent` pilot (see
`crates/agent/knowledge/pilot/README.md`), with results showing that curated
Markdown retrieval is useful enough to justify richer metadata.

## Spec-driven development: ideas adapted from Mandate

Mandate (FSL-1.1-ALv2; ideas only, no code reuse) keeps agents aligned with
specs by prompting them to follow a long rulebook. QueryMT owns the turn loop,
so the same intents become deterministic checks and delivered context instead.
Repository text stays evidence, never authority.

| Mandate idea | Repository knowledge today | Decision |
|---|---|---|
| Done gate derived from verified acceptance criteria | `tasks.md` checkboxes; the scenario-test naming rule in `openspec/config.yaml` matches 64 of 651 main-spec scenarios exactly | **Adopt**: `check` lists delta-spec scenarios of a named change with no test whose name is the scenario in snake_case (exact match on `test` symbols, no fuzzy matching). Gate only the change being archived. |
| Drift when a criterion changes after it passed | Evidence states exist, but every section inherits the whole document's `knowledge_sources` (`evidence.rs` `record_specs`) | **Adapt**: a scenario section depends on its matched test symbol; a reworded scenario becomes `unverified`, an edited test `changed`. |
| Rules scoped to the paths being edited | `knowledge_paths` and File/Purpose rows, delivered only after `index` | **Adapt after measuring** whether agents call `index` before editing (delivery counters). Extending enrichment to reads needs a retrieval-spec change. Any rule level (`must`/`should`) is a label, never a permission. |
| Reconcile specs and notes after building; blast radius | Change hints discard which paths changed (`registry.rs` `spawn_change_hints`) | **Later**: one card listing documents whose evidence dependencies changed in the turn. Needs acknowledged evidence to exist first. |
| Model-drafted content marked inferred until a person confirms | `status: draft` is never an automatic card; `acknowledge` is explicit | **Adopt as a separate command**: explicitly invoked, model-backed drafting that writes `status: draft` notes or purpose cells. Indexing and scaffold stay generation-free. |
| Stable section anchors | Heading-breadcrumb record keys; a rename orphans acknowledgements | **Optional**: an explicit anchor overrides the breadcrumb. Not needed for OpenSpec, whose headings are identity. |
| Concurrent changes touching one requirement | Nothing | **Adopt**: record a hash of the base requirement when a MODIFIED delta is written; refuse a stale delta at archive (same preimage pattern as plan/apply). |
| Commit-pinned references | Card versions are content hashes | **Skip**: content hashes are stronger on dirty worktrees. |
| Work orders, claims, heartbeats, async questions | Quorum delegation, blocking `question` tool | **Out of scope here**: agent runtime; consider with `add-git-worktree-isolation`. |
| Server, embeddings, always-on rulebook, executing rule `check` commands | None | **Reject**: conflicts with offline lexical retrieval, prompt-cache stability, and "evidence, not authority". |

Ordering: none of the adopted items should start before the delivery path has
been exercised in live sessions (task 6.4) and the gaps in
`scenario-audit.md` are closed, because the top three depend on evidence and
delivery, the least exercised parts of M1-M4.
