# Spec Delta

## Purpose

Keep portable repository documentation inspectable and maintainable through deterministic checks and safe scaffolding, and evaluate whether curated or exhaustive documentation reduces discovery work without sacrificing correctness.

## ADDED Requirements

### Requirement: Index currency and evidence review are separate

The system SHALL distinguish a current document index from the review status of authored claims. Portable evidence records SHALL bind a document, section, or file-table row's reviewed text hash to explicit dependency paths/selectors and versioned content hashes. Evidence states SHALL include unverified, unchanged, changed, and missing. Hash equality SHALL NOT be represented as proof that a statement is correct. Repository queries SHALL expose these states for authored results.

#### Scenario: Source changes but note does not
- **WHEN** a note's acknowledged dependency content changes while the note is unchanged
- **THEN** the note is labeled changed evidence even when its indexed text is current
- **AND** the system does not declare its meaning false solely because bytes changed

#### Scenario: Note changes without acknowledgement
- **WHEN** authored text changes without a matching evidence acknowledgement
- **THEN** its review state becomes unverified

#### Scenario: Evidence is missing or ambiguous
- **WHEN** an acknowledged dependency disappears or its symbol selector is ambiguous
- **THEN** the system reports missing or unresolved evidence with a diagnostic
- **AND** it does not silently select a similarly named file or symbol

### Requirement: Evidence acknowledgement is deliberate and portable

The system SHALL support previewing and explicitly applying acknowledgements for selected authored records. Applied acknowledgement metadata SHALL contain relative identities and hashes suitable for Git, not private session information or absolute checkout paths. Indexing, retrieval, scaffolding, and code-map generation SHALL NOT automatically acknowledge evidence. An apply operation SHALL reject changed preview preimages rather than certify newer unseen contents.

#### Scenario: Fresh clone checks acknowledged evidence
- **WHEN** a developer clones source documents and their acknowledgement metadata
- **THEN** evidence state can be reproduced from the checkout without the original developer's database

#### Scenario: Source changes after acknowledgement preview
- **WHEN** an acknowledgement preview is followed by a source or note edit before apply
- **THEN** apply fails with a changed-preimage diagnostic
- **AND** does not mark the new contents reviewed

### Requirement: Maintenance checks are deterministic and non-mutating

The system SHALL provide read-only checks for malformed/conflicting documents, selected-source coverage, broken relative documentation references, orphan file-table rows, evidence drift, invalid orientation entrypoints or declared path associations, and configured document/output limits. Missing per-file rows SHALL be findings only when exhaustive coverage is explicitly selected. Checks SHALL offer human-readable and machine-readable diagnostics with stable codes and relative paths, and a failing check SHALL have a nonzero executable exit status. Checking SHALL NOT rewrite descriptions, delete rows, follow remote links, or infer a definite rename from similarity.

#### Scenario: Curated corpus has no exhaustive inventory
- **WHEN** a curated-mode corpus omits per-file purpose rows
- **THEN** that omission alone is not a failed coverage check

#### Scenario: File is moved
- **WHEN** a file-table row or relative Markdown reference points to a path no longer present
- **THEN** checking reports the orphan or broken reference
- **AND** preserves the authored row and reference for review

### Requirement: Optional scaffolding never invents descriptions

The system SHALL support deterministic preview of missing per-directory inventory rows and explicit apply within the authorized root. New rows SHALL be sorted file paths with empty purpose cells. Writes SHALL be restricted to newly created directory documents or unambiguous managed inventory blocks and SHALL preserve existing authored purposes and other text. Existing files without managed blocks SHALL be left unchanged with diagnostics. Scaffolding SHALL NOT create a repository-root-wide file table, delete orphaned authored rows, or invoke an LLM.

#### Scenario: Scaffold selected source directories
- **WHEN** a user previews then explicitly applies inventory scaffolding for selected non-root directories
- **THEN** eligible missing files/rows are added with empty purpose cells
- **AND** an unchanged rerun proposes no duplicate additions

#### Scenario: Existing directory instructions lack a managed block
- **WHEN** a directory has an authored AGENTS document without an unambiguous managed inventory block
- **THEN** scaffolding leaves it byte-for-byte unchanged and reports the conflict

#### Scenario: Concurrent edit before apply
- **WHEN** an inventory target changes after its preview
- **THEN** apply rejects the stale preimage rather than overwriting the edit

### Requirement: Maintenance can run without an agent model

The system SHALL expose refresh/rebuild, map, check, scaffold, and evidence-acknowledgement operations through a typed API and an executable developer surface. Deterministic operations SHALL work without provider credentials or network access. Repository writes SHALL require explicit apply or output selection, SHALL validate path confinement and preimages, and SHALL NOT create Git commits. Local cache refresh is permitted without editing repository sources.

#### Scenario: Headless maintenance
- **WHEN** a developer runs a configured check or refresh in a headless offline environment
- **THEN** it produces the same logical results as interactive maintenance without requiring a model

### Requirement: Documentation strategies have a controlled comparison

The feature SHALL include reproducible fixtures and a documented pilot for selected `crates/agent` subtrees comparing the existing workflow, curated notes plus deterministic maps, and the same corpus/maps plus exhaustive per-file descriptions. Comparisons SHALL separate navigation and conceptual questions, use a fixed source revision, keep held-out tasks out of corpus authoring and relevance-gate tuning, and report corpus creation/update effort. On a fixed corpus, they SHALL separately compare pull-only delivery, pull plus index enrichment, and pull plus index enrichment plus orientation triggers. Tasks without known filenames SHALL exercise discovery before any index call. Exhaustive coverage SHALL NOT be enabled repository-wide by the pilot.

#### Scenario: Run deterministic retrieval evaluation
- **WHEN** the fixed navigation and documentation fixtures are evaluated
- **THEN** reports identify corpus variant, source revision, index version, query class, recall/ranking metrics, and output sizes
- **AND** the retrieval evaluation runs without model access

#### Scenario: Run optional agent comparison
- **WHEN** a developer opts into model-backed task evaluation
- **THEN** the report records model/configuration, repeated trials, tool and token costs, time to relevant inspection, task correctness, and maintenance effort
- **AND** a reduction in tool calls alone is not treated as proof of improvement
- **AND** delivery modes use the same corpus so delivery effects are not confused with additional documentation

#### Scenario: Historical sessions inform fixtures
- **WHEN** local session history is used to identify pilot questions
- **THEN** committed fixtures contain only reviewed, sanitized task descriptions and source expectations
- **AND** no private transcript, session database, credentials, or developer-specific database dependency is included

#### Scenario: Replay delivery triggers
- **WHEN** fixed prompt/tool-event fixtures exercise positive and negative delivery cases
- **THEN** reports distinguish matched, delivered, suppressed, and fetched cards and record false-positive rate, missed cues, duplicates, added context tokens, and latency
- **AND** fixtures include path-free feature requests, unrelated prompts, generic terms, outside-root files, parallel calls, and compaction/resume
- **AND** delivered or fetched cards are not automatically counted as followed

### Requirement: Correctness and portability gate rollout

Before enabling the feature in an example coding profile, validation SHALL cover clone/rebuild equivalence, changed/deleted sources, branch and linked-worktree isolation, bounded outputs, private-memory separation, deterministic generation, safe failed refresh, legacy-tool compatibility, path-free orientation, unchanged raw index output, output-policy preservation, shared delivery budgets, retry/resume deduplication, and compaction-aware revalidation. Pilot results and limitations SHALL be documented even if neither documentation strategy demonstrates a performance improvement.

#### Scenario: Candidate is faster but leaks private knowledge
- **WHEN** an evaluation variant reduces discovery cost but violates isolation or correctness gates
- **THEN** it is not eligible for rollout

#### Scenario: Results are inconclusive
- **WHEN** the pilot does not establish an advantage for exhaustive documentation
- **THEN** the report records the uncertainty and maintenance costs
- **AND** no automatic repository-wide exhaustive inventory rollout occurs
