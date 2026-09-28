# Spec Delta

## Purpose

Provide portable, repository-owned knowledge that developers can share through source control and rebuild into a private, checkout-local search index without exchanging session history.

## ADDED Requirements

### Requirement: Repository knowledge is explicitly enabled and configured

The system SHALL require host opt-in and a supported `.agents/knowledge.toml` source manifest before indexing repository knowledge. The manifest SHALL select documentation and code using repository-relative include/exclude patterns, with excludes taking precedence. Repository content SHALL NOT enable the feature, grant tools, or expand host filesystem access. Disabled or absent configuration SHALL preserve existing behavior and SHALL NOT create repository files.

#### Scenario: Feature is disabled
- **WHEN** a workspace contains a knowledge manifest but host repository knowledge is disabled
- **THEN** the manifest does not trigger indexing, prompt changes, or file creation
- **AND** legacy knowledge operations remain unchanged

#### Scenario: Enabled feature has no manifest
- **WHEN** repository knowledge is enabled but the selected root has no manifest
- **THEN** the system reports that repository knowledge is not configured
- **AND** it does not implicitly crawl the repository or create a manifest

#### Scenario: Unsupported manifest version
- **WHEN** the manifest declares an unsupported format version
- **THEN** repository indexing is unavailable with an actionable diagnostic
- **AND** source files and legacy knowledge are not modified

### Requirement: Authoritative knowledge is portable text

The system SHALL accept selected Markdown, including workspace `.agents/memories/*.md`, architecture documents, and directory `AGENTS.md` files, without requiring a developer's database. Shared configuration, citations, document identities, and optional evidence metadata SHALL use repository-relative references rather than machine-local paths or session IDs. Explicit document IDs SHALL survive moves; documents without an explicit ID SHALL use path-derived identity.

#### Scenario: Another developer clones the repository
- **WHEN** two developers index identical configured source contents in different checkout paths using the same index format
- **THEN** they obtain equivalent logical documents, navigation records, and relative citations
- **AND** neither needs the other's database, credentials, or conversations

#### Scenario: Identified document moves
- **WHEN** a document with an explicit ID moves within the configured corpus and retains its ID
- **THEN** its document identity remains stable and its citation updates to the new relative path

### Requirement: Optional delivery metadata is portable and declarative

The manifest SHALL support optional selected-document or section entrypoints for initial orientation. Documents SHALL support optional repository-relative path associations distinct from evidence dependencies. Invalid entrypoints or associations SHALL produce diagnostics without hiding unrelated valid sources. Delivery metadata SHALL NOT contain executable hook rules, enable automatic delivery, expand indexed coverage, or grant permissions. Associated document bodies SHALL remain ordinary authored Markdown and need no generated trigger prose.

#### Scenario: Repository supplies an orientation entrypoint
- **WHEN** a selected repository-map document is referenced by the optional orientation configuration
- **THEN** its relative document/section identity is usable after a fresh clone
- **AND** it is not automatically injected unless the host enables the orientation delivery stage

#### Scenario: Directory association does not acknowledge evidence
- **WHEN** a note declares applicability to a repository-relative source-directory glob
- **THEN** that association can select it for authorized index enrichment
- **AND** the association alone neither marks its claims reviewed nor permits reading excluded files

### Requirement: Discovery is confined and diagnosable

The system SHALL confine discovery and reads to the authorized knowledge root, reject traversal and external symlink escapes, and exclude ignored content unless the host explicitly allows it. Selected hidden documentation SHALL be supported. The system SHALL enforce host file/count/byte limits and disclose incomplete coverage. Invalid documents and conflicting IDs SHALL produce source-specific diagnostics without hiding valid siblings; overlapping source patterns SHALL NOT duplicate documents.

#### Scenario: Selected hidden memory is indexed
- **WHEN** the manifest selects a valid, non-ignored `.agents/memories/runtime.md` within the root
- **THEN** it is discoverable despite being under a hidden directory

#### Scenario: Symlink escapes the root
- **WHEN** a selected document resolves through a symlink outside the authorized root
- **THEN** no external contents are read or indexed
- **AND** a diagnostic identifies the rejected repository-relative source

#### Scenario: Conflicting IDs or size limits
- **WHEN** two documents use the same explicit ID or a selected file exceeds host limits
- **THEN** affected documents are excluded with diagnostics
- **AND** valid unrelated documents remain available with incomplete-coverage information

### Requirement: Knowledge belongs to the effective checkout

The system SHALL resolve repository knowledge from the effective session checkout, using an explicitly authorized root or the containing Git worktree root. A nested package working directory SHALL NOT silently create a different repository collection. Non-Git workspaces SHALL require an explicit root. Linked worktrees SHALL have independent active index state even when they share Git history.

#### Scenario: Session starts inside a crate
- **WHEN** one session starts at the repository root and another at `crates/agent` in the same checkout without an overriding root
- **THEN** both resolve the same repository knowledge manifest and collection

#### Scenario: Worktrees differ
- **WHEN** two linked worktrees contain different versions of an indexed document
- **THEN** each session retrieves the document from its own effective worktree
- **AND** refreshing one does not overwrite the other's active corpus

### Requirement: Derived indexes refresh without modifying their sources

The system SHALL persist a locally rebuildable index and reconcile current configured files before repository retrieval. Additions, edits, removals, disabled documents, manifest changes, and branch changes SHALL invalidate affected records. Unchanged parsed records SHALL be reusable. Retrieval SHALL expose only a complete index generation or an explicit unavailable diagnostic, never silently serve known outdated content as current. Indexing SHALL NOT write Markdown or acknowledgement metadata.

#### Scenario: External edit without a watcher event
- **WHEN** an indexed file changes outside QueryMT and the next repository query runs without a delivered filesystem event
- **THEN** the changed content is detected and reconciled before it is presented as current

#### Scenario: Source is removed or becomes invalid
- **WHEN** a formerly indexed document is deleted, disabled, excluded, or malformed
- **THEN** its old body is excluded from live repository retrieval
- **AND** malformed-source diagnostics distinguish failure from a valid empty corpus

#### Scenario: Refresh overlaps another reader
- **WHEN** a repository refresh runs while another process or session reads the collection
- **THEN** the reader observes a complete generation or a disclosed updating/unavailable condition
- **AND** it does not observe partially reconciled documents and search records

#### Scenario: Cache is lost
- **WHEN** a local index is missing or corrupt
- **THEN** it can be rebuilt solely from configured source files and portable metadata
- **AND** authoritative files and private knowledge are unchanged

### Requirement: Repository projection does not publish private knowledge

Repository retrieval SHALL contain only authorized configured repository sources and generated records. It SHALL NOT automatically include private session entries, global protocol memories, or legacy consolidations. Existing protocol memory imports and session retention/consolidation behavior SHALL remain compatible. Repository indexing SHALL NOT ingest into or deactivate records owned by the legacy protocol loader.

#### Scenario: Workspace memory is also imported by the protocol loader
- **WHEN** both repository indexing and protocol loading select the same workspace memory
- **THEN** repository retrieval contains one repository projection of that file
- **AND** legacy import behavior remains unchanged

#### Scenario: Private entry mentions a repository symbol
- **WHEN** a private session entry or consolidation matches a repository query
- **THEN** it is not returned by the repository collection
- **AND** rebuilding or deleting the repository cache does not alter that private entry
