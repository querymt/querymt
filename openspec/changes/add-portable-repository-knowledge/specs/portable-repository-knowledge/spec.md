# Spec Delta

## Purpose

Provide portable, repository-owned knowledge that developers can share through source control and rebuild into a private, checkout-local search index without exchanging session history.

## ADDED Requirements

### Requirement: Repository knowledge is explicitly enabled and configured

The system SHALL require host opt-in and a supported `.qmt/knowledge.toml` source manifest at the selected root before indexing repository knowledge. The manifest SHALL select documentation and code using repository-relative include/exclude patterns, with excludes taking precedence. Repository content SHALL NOT enable the feature, grant tools, or expand host filesystem access. Disabled or absent configuration SHALL preserve existing behavior and SHALL NOT create repository files.

#### Scenario: Feature is disabled
- **WHEN** a workspace contains a knowledge manifest but host repository knowledge is disabled
- **THEN** the manifest does not trigger indexing, prompt changes, or file creation
- **AND** legacy knowledge operations remain unchanged

#### Scenario: Enabled feature has no manifest
- **WHEN** repository knowledge is enabled but the selected root has no manifest
- **THEN** the system reports that repository knowledge is not configured
- **AND** it does not implicitly crawl the repository or create a manifest

#### Scenario: Unsupported manifest version or table
- **WHEN** the manifest declares an unsupported format version or an unknown top-level table
- **THEN** repository indexing is unavailable with an actionable diagnostic
- **AND** source files and legacy knowledge are not modified

### Requirement: QueryMT control files do not occupy the shared .agents namespace

The system SHALL keep QueryMT-specific repository knowledge control files (the source manifest and evidence acknowledgements) under the project `.qmt/` directory. The system SHALL NOT read a knowledge manifest from, or write any repository knowledge file into, `.agents/`. Selecting existing `.agents/memories/*.md` files as documentation inputs SHALL remain supported and SHALL NOT change `dotagents-protocol` discovery, layering, or import behavior.

#### Scenario: Manifest placed under .agents
- **WHEN** a repository contains `.agents/knowledge.toml` but no `.qmt/knowledge.toml`
- **THEN** repository knowledge reports not configured
- **AND** the `.agents` file is not interpreted as a repository knowledge manifest

#### Scenario: Protocol memories are selected as documents
- **WHEN** `.qmt/knowledge.toml` selects `.agents/memories/*.md` and the dotagents protocol is also enabled
- **THEN** both systems read the files independently
- **AND** no file is created or modified under `.agents/`

### Requirement: Authoritative knowledge is portable text

The system SHALL accept selected Markdown, including workspace `.agents/memories/*.md`, architecture documents, and directory `AGENTS.md` files, without requiring a developer's database. Shared configuration, citations, document identities, and evidence metadata SHALL use repository-relative references rather than machine-local paths or session IDs. Explicit document IDs SHALL survive moves; documents without an explicit ID SHALL use path-derived identity.

#### Scenario: Another developer clones the repository
- **WHEN** two developers index identical configured source contents in different checkout paths using the same index format
- **THEN** they obtain equivalent logical documents, navigation records, and relative citations
- **AND** neither needs the other's database, credentials, or conversations

#### Scenario: Identified document moves
- **WHEN** a document with an explicit ID moves within the configured corpus and retains its ID
- **THEN** its document identity remains stable and its citation updates to the new relative path

### Requirement: Document frontmatter is optional and uses existing conventions

Documents SHALL NOT require frontmatter. When present, frontmatter SHALL be parsed with the existing dotagents frontmatter conventions. The recognized optional keys SHALL be `id`, `title`, `description`, `tags`, `status`, `knowledge_sources`, and `knowledge_paths`. Unknown keys SHALL be retained and ignored. Workspace memories SHALL keep their legacy parsing and enabled semantics.

#### Scenario: Plain Markdown and legacy memory coexist
- **WHEN** selected inputs contain plain Markdown without frontmatter, Markdown with unknown frontmatter keys, and a legacy memory
- **THEN** all valid inputs are available through one repository projection with original source coordinates
- **AND** existing memory parsing and enabled semantics remain unchanged

### Requirement: Optional delivery metadata is portable and declarative

The manifest SHALL support optional selected-document or section entrypoints for initial orientation. Documents SHALL support optional `knowledge_paths` repository-relative path associations distinct from `knowledge_sources` evidence dependencies. Invalid entrypoints or associations SHALL produce diagnostics without hiding unrelated valid sources. Delivery metadata SHALL NOT contain executable hook rules, enable automatic delivery, expand indexed coverage, or grant permissions.

#### Scenario: Repository supplies an orientation entrypoint
- **WHEN** a selected repository-map document is referenced by the optional orientation configuration
- **THEN** its relative document/section identity is usable after a fresh clone
- **AND** it is not automatically injected unless the host enables the orientation delivery stage

#### Scenario: Directory association does not acknowledge evidence
- **WHEN** a note declares applicability to a repository-relative source-directory glob
- **THEN** that association can select it for authorized index enrichment
- **AND** the association alone neither marks its claims acknowledged nor permits reading excluded files

### Requirement: Discovery is confined and diagnosable

The system SHALL confine discovery and reads to the authorized knowledge root, reject traversal and external symlink escapes, and exclude ignored content unless the host explicitly allows it. Selected hidden documentation SHALL be supported. The system SHALL enforce host file/count/byte limits and disclose incomplete coverage. Overlapping source patterns SHALL NOT duplicate documents.

#### Scenario: Selected hidden memory is indexed
- **WHEN** the manifest selects a valid, non-ignored `.agents/memories/runtime.md` within the root
- **THEN** it is discoverable despite being under a hidden directory

#### Scenario: Symlink escapes the root
- **WHEN** a selected document resolves through a symlink outside the authorized root
- **THEN** no external contents are read or indexed
- **AND** a diagnostic identifies the rejected repository-relative source

### Requirement: Invalid sources are isolated

Invalid documents, malformed frontmatter, conflicting explicit IDs, and over-limit files SHALL be excluded with source-specific diagnostics. Valid sibling documents SHALL remain available, and coverage SHALL be reported as incomplete.

#### Scenario: Conflicting IDs or size limits
- **WHEN** two documents use the same explicit ID or a selected file exceeds host limits
- **THEN** affected documents are excluded with diagnostics
- **AND** valid unrelated documents remain available with incomplete-coverage information

#### Scenario: Malformed frontmatter
- **WHEN** one selected document has unparseable frontmatter
- **THEN** only that document is excluded with a diagnostic

### Requirement: Document references do not expand authorization

Relative Markdown references SHALL resolve from their containing document and remain confined to the authorized checkout. Links SHALL NOT select extra index inputs or grant access. External URLs SHALL remain inert metadata during indexing and checking.

#### Scenario: Reference escapes or names an external URL
- **WHEN** a reference escapes the checkout or names an external URL
- **THEN** escaping filesystem reads are rejected and the URL is not fetched automatically

### Requirement: Knowledge belongs to the effective checkout

The system SHALL resolve repository knowledge from the effective session checkout, using an explicitly authorized root or the containing Git worktree root. A nested package working directory SHALL NOT silently create a different repository collection. Non-Git workspaces SHALL require an explicit root. Linked worktrees SHALL have independent active index state even when they share Git history.

#### Scenario: Session starts inside a crate
- **WHEN** one session starts at the repository root and another at `crates/agent` in the same checkout without an overriding root
- **THEN** both resolve the same repository knowledge manifest and collection

#### Scenario: Worktrees differ
- **WHEN** two linked worktrees contain different versions of an indexed document
- **THEN** each session retrieves the document from its own effective worktree
- **AND** refreshing one does not overwrite the other's active corpus

### Requirement: The local index lives under the QueryMT config directory

The local index SHALL be stored under the QueryMT config directory resolved by the shared config-directory helper, keyed by checkout, and SHALL NOT be written into the repository or `sessions.db`. Public citations and portable IDs SHALL NOT contain the machine-local key.

#### Scenario: Config directory is overridden
- **WHEN** the host resolves a non-default QueryMT config directory
- **THEN** the repository index is created under that directory rather than a hardcoded home path

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

### Requirement: Freshness checks avoid hashing unchanged files

Reconciliation SHALL compare stored file size, modification time, and file identity before reading content. It SHALL hash content only when any of these differ, when the stored modification time is within a racy-timestamp window of the previous index time, or when reliable metadata is unavailable. Content SHALL be reparsed only when its hash changed. Sources SHALL be revalidated by the same rule before a body or coordinates are emitted.

#### Scenario: Warm query with no changes
- **WHEN** a repository query runs and no selected file's metadata has changed outside the racy window
- **THEN** reconciliation completes without reading selected file contents

#### Scenario: Same-size edit within the racy window
- **WHEN** a file is rewritten with the same size and a modification time within the racy-timestamp window of the last index
- **THEN** its content is hashed and the edit is detected

#### Scenario: Metadata changes but content does not
- **WHEN** a file's modification time changes but its content hash is unchanged
- **THEN** its parsed records are reused without reparsing

### Requirement: Hosts can force full content hashing

A host setting SHALL force content hashing of every selected file during reconciliation, bypassing the metadata fast path.

#### Scenario: Forced hashing is enabled
- **WHEN** the host enables forced hashing and no file metadata has changed
- **THEN** every selected file's content is hashed during reconciliation

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
