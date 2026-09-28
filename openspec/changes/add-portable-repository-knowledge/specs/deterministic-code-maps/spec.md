# Spec Delta

## Purpose

Provide reproducible, compact source-navigation records so agents can locate relevant files and symbols without repeatedly rediscovering repository structure or relying on generated semantic claims.

## ADDED Requirements

### Requirement: Code maps contain source-derived structural facts

For configured supported source files, the system SHALL produce file and symbol records containing repository-relative paths, language, declaration kind, qualified name, signature, parent identity where available, original source ranges, and content fingerprints. Declared imports SHALL be represented as syntactic declarations rather than asserted resolved dependencies. Generated records SHALL NOT invent purposes, invariants, call edges, or ownership from filenames or model output.

#### Scenario: Rust module is indexed
- **WHEN** a configured Rust source contains a struct, an implementation, methods, and imports
- **THEN** its navigation records expose the corresponding declarations, signatures, relationships, and original file ranges
- **AND** they do not claim semantic behavior not directly extracted from source

#### Scenario: Existing Rust documentation is available
- **WHEN** supported Rust module or item documentation comments are extracted
- **THEN** their text is preserved verbatim with source coordinates and identified as source documentation
- **AND** it is not represented as a newly inferred purpose

### Requirement: Identical source produces reproducible maps

Given identical selected source bytes, configuration, and extractor format version, map generation SHALL produce equivalent records and byte-identical portable map exports across checkout paths. Portable output SHALL use deterministic path/symbol ordering and SHALL NOT include machine-local paths or volatile timestamps. Generation SHALL require no LLM, provider credentials, network service, or prior session history.

#### Scenario: Fresh clone generates the same map
- **WHEN** two different local checkout paths contain the same configured source and use the same extractor version
- **THEN** their portable map exports are byte-identical

#### Scenario: Offline generation
- **WHEN** map generation runs without network access or model credentials
- **THEN** supported structural extraction and rendering remain available

### Requirement: Incremental records track source and extractor changes

The system SHALL reuse unchanged extracted records and invalidate records when source bytes, selected paths, or extractor format change. Deleted files and symbols SHALL disappear from active navigation after reconciliation. Source edits SHALL NOT automatically acknowledge authored descriptions as reviewed.

#### Scenario: One file changes
- **WHEN** one selected file changes while other selected source bytes and extractor version remain unchanged
- **THEN** its generated records and ranges are refreshed
- **AND** unchanged files reuse their existing extraction results

#### Scenario: Extractor version changes
- **WHEN** a new extractor format changes record interpretation
- **THEN** incompatible cached records are regenerated rather than mixed with the new format

### Requirement: Partial extraction is explicit

Unsupported languages, parse failures, and host size limits SHALL be visible through path-level records or coverage diagnostics as appropriate. The system SHALL NOT describe partial extraction as a complete semantic index or omit skipped files without explanation. Out-of-policy sources SHALL remain unread.

#### Scenario: Unsupported selected source
- **WHEN** a selected regular source file has an unsupported language
- **THEN** navigation can still identify its path and unsupported status
- **AND** no fabricated symbol or purpose is created

### Requirement: Maps are bounded and do not rewrite authored content

The system SHALL provide bounded directory/file map retrieval and explicit portable export. Over-budget maps SHALL disclose omitted records and narrowing options. Normal indexing SHALL NOT write generated maps into repository documents. Explicit export SHALL replace only a designated generated artifact or managed block after preimage validation and SHALL preserve authored content outside it.

#### Scenario: Large directory map
- **WHEN** a requested directory map exceeds its output budget
- **THEN** it returns a bounded map with truncation and narrower-path guidance

#### Scenario: Export into an authored document
- **WHEN** an export targets an existing document without an unambiguous managed block
- **THEN** it refuses to overwrite that document and reports how to select a safe generated destination

### Requirement: Existing source tools retain their contracts

Introducing repository code maps SHALL preserve existing `index` outline formatting, supported-language behavior, whole-file coordinate guarantees, and the source-read/edit-precondition behavior of symbol tools. Cached maps SHALL be navigation aids rather than substitutes for current-source validation before an edit. Opt-in post-index knowledge enrichment SHALL be separate attributed context, not a change to the raw outline or a second structural summary.

#### Scenario: Existing outline fixture
- **WHEN** an existing supported-language outline test runs with repository knowledge enabled or disabled
- **THEN** its established raw outline output remains unchanged
- **AND** enabling knowledge enrichment adds only separately attributed context outside that outline

#### Scenario: Source changed after map retrieval
- **WHEN** a file changes after its map was fetched and an edit is attempted
- **THEN** the existing source validation and stale-write protections still apply
- **AND** the cached map does not authorize an edit against old bytes
