# Spec Delta

## Purpose

Keep portable repository documentation inspectable and maintainable through deterministic checks and safe scaffolding, and evaluate whether curated or exhaustive documentation reduces discovery work without sacrificing correctness.

## ADDED Requirements

### Requirement: Index currency and evidence acknowledgement are separate

The system SHALL distinguish a current document index from the hash-bound evidence acknowledgement of authored claims. Evidence records SHALL bind a document, section, or file-table row's reviewed text hash to explicit `knowledge_sources` dependency selectors and versioned content hashes. Evidence states SHALL be unverified, unchanged, changed, or missing. Hash equality SHALL NOT be represented as proof that a statement is correct, and lifecycle status SHALL NOT alter evidence state.

#### Scenario: Source changes but note does not
- **WHEN** a note's acknowledged dependency content changes while the note is unchanged
- **THEN** the note is labeled changed evidence even when its indexed text is current
- **AND** the system does not declare its meaning false solely because bytes changed

#### Scenario: Note changes without acknowledgement
- **WHEN** authored text changes without a matching evidence acknowledgement
- **THEN** its evidence state becomes unverified

#### Scenario: Evidence is missing or ambiguous
- **WHEN** an acknowledged dependency disappears or its symbol selector is ambiguous
- **THEN** the system reports missing or unresolved evidence with a diagnostic
- **AND** it does not silently select a similarly named file or symbol

### Requirement: Evidence is stored per document under .qmt

Evidence acknowledgements SHALL be stored as one Git-trackable file per acknowledged document at `.qmt/knowledge/evidence/<document-relative-path>.json`. Each file SHALL carry a format version, be serialized with sorted keys and a trailing newline, and contain only repository-relative identities and hashes. An evidence file whose document no longer exists SHALL be reported as orphaned.

#### Scenario: Independent branches acknowledge different notes
- **WHEN** two branches each acknowledge a different document and are merged
- **THEN** their acknowledgements are in different files and merge without textual conflict

#### Scenario: Document is moved
- **WHEN** an acknowledged document moves without its evidence file
- **THEN** check reports the orphaned evidence file and the moved document as unverified

#### Scenario: Evidence file content is portable
- **WHEN** evidence is acknowledged on one machine
- **THEN** the file contains no absolute paths, session IDs, private content, or timestamps required for equality

### Requirement: Evidence hashes are line-ending and whitespace stable

Text hashes for reviewed sections and whole-file dependencies SHALL be computed after stripping a UTF-8 byte-order mark, normalizing CRLF and CR to LF, and removing trailing whitespace on each line. The normalization SHALL be identified by the hash version. Symbol dependencies SHALL use the extractor's existing digest.

#### Scenario: Clone with different line endings
- **WHEN** a repository is checked out with CRLF line endings after acknowledgement on an LF checkout
- **THEN** unchanged notes and dependencies report unchanged evidence

#### Scenario: Substantive edit
- **WHEN** a dependency's non-whitespace content changes
- **THEN** its evidence state becomes changed

### Requirement: Evidence acknowledgement is deliberate and portable

The system SHALL support previewing and explicitly applying acknowledgements for selected authored records. Indexing, retrieval, scaffolding, and code-map generation SHALL NOT automatically acknowledge evidence. An apply operation SHALL reject changed preview preimages rather than certify newer unseen contents. Bulk acknowledgement of all drift SHALL NOT be offered implicitly.

#### Scenario: Fresh clone checks acknowledged evidence
- **WHEN** a developer clones source documents and their evidence files
- **THEN** evidence state can be reproduced from the checkout without the original developer's database

#### Scenario: Source changes after acknowledgement preview
- **WHEN** an acknowledgement preview is followed by a source or note edit before apply
- **THEN** apply fails with a changed-preimage diagnostic
- **AND** does not acknowledge the new contents

### Requirement: Maintenance checks are deterministic and non-mutating

The system SHALL provide read-only checks for malformed/conflicting documents, source coverage, broken relative references, orphan file-table rows, evidence drift, orphaned evidence files, invalid entrypoints or `knowledge_paths`, and configured limits. Checking SHALL NOT rewrite content, delete rows, follow remote links, or infer a definite rename from similarity.

#### Scenario: File is moved
- **WHEN** a file-table row or relative Markdown reference points to a path no longer present
- **THEN** checking reports the orphan or broken reference
- **AND** preserves the authored row and reference for review

#### Scenario: Document includes unknown metadata and an external URL
- **WHEN** an otherwise valid document has unknown frontmatter keys and links to an external URL
- **THEN** checks do not fail on the unknown keys and make no network request

### Requirement: Check strictness matches the selected coverage mode

Missing per-file rows SHALL be findings only when exhaustive coverage is explicitly selected. Missing optional listings and unknown frontmatter keys SHALL NOT alone fail a check.

#### Scenario: Curated corpus has no exhaustive inventory
- **WHEN** a curated-mode corpus omits per-file purpose rows
- **THEN** that omission alone is not a failed coverage check

### Requirement: Check output is machine-readable and gates CI

Checks SHALL offer human-readable and machine-readable diagnostics with stable codes and relative paths. A failing check SHALL have a nonzero executable exit status.

#### Scenario: Check fails in CI
- **WHEN** a check reports a broken reference in JSON mode
- **THEN** the diagnostic has a stable code and relative path and the process exits nonzero

### Requirement: Optional scaffolding never invents descriptions

The system SHALL support deterministic preview of missing per-directory inventory rows and explicit apply within the authorized root. New rows SHALL be sorted file paths with empty purpose cells, and existing authored purposes SHALL be preserved. Scaffolding SHALL NOT create a repository-root-wide file table, delete orphaned authored rows, or invoke an LLM.

#### Scenario: Scaffold selected source directories
- **WHEN** a user previews then explicitly applies inventory scaffolding for selected non-root directories
- **THEN** eligible missing files/rows are added with empty purpose cells
- **AND** an unchanged rerun proposes no duplicate additions

#### Scenario: Concurrent edit before apply
- **WHEN** an inventory target changes after its preview
- **THEN** apply rejects the stale preimage rather than overwriting the edit

### Requirement: Scaffold writes are limited to managed blocks

Scaffold writes SHALL be restricted to newly created directory documents or managed blocks delimited by `<!-- qmt:inventory:begin -->` and `<!-- qmt:inventory:end -->`, preserving all text outside the block. Managed blocks SHALL contain only a path/purpose table and no instructions. Files without exactly one managed block SHALL be left unchanged with a diagnostic.

#### Scenario: Existing directory instructions lack a managed block
- **WHEN** a directory has an authored AGENTS document without an unambiguous managed inventory block
- **THEN** scaffolding leaves it byte-for-byte unchanged and reports the conflict

### Requirement: Maintenance can run without an agent model

The system SHALL expose refresh/rebuild, map, check, scaffold, and evidence-acknowledgement operations through a typed API and an executable developer surface. Deterministic operations SHALL work without provider credentials or network access.

#### Scenario: Headless maintenance
- **WHEN** a developer runs a configured check or refresh in a headless offline environment
- **THEN** it produces the same logical results as interactive maintenance without requiring a model

### Requirement: Maintenance writes are explicit and confined

Repository writes SHALL require explicit apply or output selection, SHALL validate path confinement and preimages, SHALL write atomically within the authorized root and never under `.agents/`, and SHALL NOT create Git commits. Local cache refresh SHALL NOT edit repository sources.

#### Scenario: Apply targets .agents
- **WHEN** a maintenance apply would write a file under `.agents/`
- **THEN** the write is refused with a diagnostic

### Requirement: Documentation strategies have a controlled comparison

The feature SHALL include reproducible fixtures and a documented pilot for selected `crates/agent` subtrees comparing the existing workflow, curated notes plus deterministic maps, and the same corpus/maps plus exhaustive per-file descriptions. Comparisons SHALL use a fixed source revision, separate navigation and conceptual questions, and keep held-out tasks out of corpus authoring and gate tuning. The pilot SHALL NOT enable exhaustive coverage repository-wide.

#### Scenario: Run deterministic retrieval evaluation
- **WHEN** the fixed navigation and documentation fixtures are evaluated
- **THEN** reports identify corpus variant, source revision, index version, gate version, query class, recall/ranking metrics, and output sizes
- **AND** the retrieval evaluation runs without model access

#### Scenario: Run optional agent comparison
- **WHEN** a developer opts into model-backed task evaluation
- **THEN** the report records model/configuration, repeated trials, tool and token costs, time to relevant inspection, task correctness, and maintenance effort
- **AND** a reduction in tool calls alone is not treated as proof of improvement
- **AND** delivery modes use the same corpus so delivery effects are not confused with additional documentation

### Requirement: Delivery modes are compared on a fixed corpus

On a fixed corpus, the pilot SHALL separately compare pull-only delivery, pull plus index enrichment, and pull plus index enrichment plus orientation triggers, and SHALL report corpus authoring/update effort and cold/warm refresh cost.

#### Scenario: Delivery comparison report
- **WHEN** the three delivery modes are evaluated on the same corpus
- **THEN** the report attributes differences to delivery mode and records authoring effort and refresh cost

#### Scenario: Historical sessions inform fixtures
- **WHEN** local session history is used to identify pilot questions
- **THEN** committed fixtures contain only reviewed, sanitized task descriptions and source expectations
- **AND** no private transcript, session database, credentials, or developer-specific database dependency is included

#### Scenario: Replay delivery triggers
- **WHEN** fixed prompt/tool-event fixtures exercise positive and negative delivery cases
- **THEN** reports distinguish matched, delivered, suppressed, and fetched cards and record false-positive rate, missed cues, duplicates, added context tokens, and latency
- **AND** fixtures include path-free feature requests, unrelated prompts, generic terms, non-English prompts, outside-root files, parallel calls, and compaction/resume

### Requirement: Correctness and portability gate rollout

Each milestone SHALL pass its stage-specific gate before it is enabled in an example profile. The base gate SHALL cover clone/rebuild equivalence, changed/deleted sources, fast-path detection, worktree isolation, bounded outputs, private-memory separation, no writes under `.agents/`, deterministic generation, safe failed refresh, and legacy-tool compatibility. Pilot results and limitations SHALL be documented even without a demonstrated improvement.

#### Scenario: Candidate is faster but leaks private knowledge
- **WHEN** an evaluation variant reduces discovery cost but violates isolation or correctness gates
- **THEN** it is not eligible for rollout

#### Scenario: Results are inconclusive
- **WHEN** the pilot does not establish an advantage for exhaustive documentation
- **THEN** the report records the uncertainty and maintenance costs
- **AND** no automatic repository-wide exhaustive inventory rollout occurs

### Requirement: Delivery stages have an additional rollout gate

Before an automatic delivery stage is enabled in an example profile, validation SHALL additionally cover path-free orientation, unchanged raw index output, output-policy preservation, shared delivery budgets, retry/resume deduplication, and compaction-aware revalidation.

#### Scenario: Delivery stage fails dedup gate
- **WHEN** retry/resume replay reinjects an unchanged card
- **THEN** that delivery stage is not enabled in an example profile
