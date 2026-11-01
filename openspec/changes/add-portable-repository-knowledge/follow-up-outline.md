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
