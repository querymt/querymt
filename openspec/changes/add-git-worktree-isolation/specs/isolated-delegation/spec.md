# Spec Delta

## Purpose

Allow parallel delegates to work from their parent's current code state in separate checkouts, with correct verification and reviewable results that do not overwrite parent work.

## ADDED Requirements

### Requirement: Resolved delegation isolation policy
The system SHALL support explicit isolated delegation from local or isolated parents. Delegates of isolated sessions SHALL default to isolated execution; existing local sessions SHALL retain shared execution unless isolation is configured or requested. The resolved policy SHALL be persisted and disclosed. Requested isolation SHALL NOT fall back to sharing on failure.

#### Scenario: Parallel delegates of an isolated parent
- **WHEN** an isolated parent dispatches two delegates without overriding the isolation policy
- **THEN** each delegate receives its own checkout distinct from the parent and sibling
- **AND** concurrent writes to the same relative path do not affect either other checkout

#### Scenario: Isolated delegate of a local parent
- **WHEN** a local parent explicitly requests an isolated delegate
- **THEN** the delegate runs in its own checkout and the parent remains local

#### Scenario: Explicit shared override
- **WHEN** permitted policy explicitly selects shared execution
- **THEN** the operation is labeled shared rather than isolated

### Requirement: Private baseline includes parent edits
An isolated delegate SHALL start from a validated immutable baseline containing the parent's tracked working-file contents, tracked deletions, and non-ignored untracked files. The capture SHALL preserve supported Git file semantics including executable bits, symlinks, binary contents, and tracked hidden files. It SHALL NOT modify the parent's HEAD, branch, index, stash, or working files. It SHALL exclude ignored untracked files and disclose that exclusion. The baseline SHALL reflect working contents rather than reproduce the staging partition.

#### Scenario: Parent has partially staged edits
- **WHEN** the parent has different staged and unstaged contents for a file and starts an isolated delegate
- **THEN** the delegate sees the working-file contents
- **AND** the parent's staged contents, working contents, HEAD, and stash remain unchanged

#### Scenario: Mixed local changes
- **WHEN** parent changes include a tracked deletion, a tracked dotfile, a binary file, an executable, a symlink, a non-ignored new file, and an ignored untracked environment file
- **THEN** the supported tracked and non-ignored changes appear in the delegate baseline with their Git file semantics preserved
- **AND** the ignored environment file is not copied and the exclusion is disclosed

#### Scenario: Baseline equals HEAD
- **WHEN** the parent's captured working contents equal HEAD
- **THEN** the delegate baseline identifies that committed state without requiring a parent commit

### Requirement: Consistent baseline capture and provenance
Delegates dispatched as one parallel batch SHALL use the same immutable baseline. Independently requested delegates SHALL capture the then-current parent state. Capture SHALL coordinate with cooperating QueryMT mutations, validate for observed concurrent external changes, and retry or fail instead of launching from a known inconsistent capture. Unsupported or conflicted repository states SHALL fail explicitly before child execution rather than omit content silently.

#### Scenario: Siblings share starting contents
- **WHEN** two delegates are dispatched in one batch
- **THEN** both report the same baseline identity while using different writable checkouts

#### Scenario: Parent changes after launch
- **WHEN** the parent edits a file after a delegate baseline has been captured
- **THEN** the running delegate's baseline and checkout are unaffected

#### Scenario: Change detected during capture
- **WHEN** source validation detects that files, HEAD, or index changed during capture
- **THEN** the system retries with a new validated capture or returns an actionable failure without launching the child

#### Scenario: Unsupported source state
- **WHEN** the source has unresolved index conflicts or an unsupported repository feature that prevents faithful materialization
- **THEN** delegation fails before execution and identifies the unsupported state

### Requirement: Child-workspace verification and execution context
The system SHALL run delegate tools and verification against the child's effective workspace. Execution-related hook context SHALL distinguish the parent and child workspace; pre-allocation policy hooks SHALL identify parent context explicitly. Workspace-dependent integrations SHALL use the child root or fail explicitly.

#### Scenario: Verification distinguishes child from parent
- **WHEN** a delegate changes code so that a verification check has a different result in its checkout than in the parent checkout
- **THEN** the reported verification outcome is the child's outcome

#### Scenario: Hooks inspect a delegate
- **WHEN** a hook runs for an allocated delegate's execution or completion
- **THEN** its context identifies the child's execution path and the parent association without substituting the orchestrator default cwd

### Requirement: Durable baseline-relative results
A completed isolated delegate SHALL report its workspace, branch, baseline identity, current HEAD, verification status, and changes relative to the baseline. Uncommitted results SHALL be preserved as an immutable reviewable result when capture succeeds, without modifying the child's real staging state. Completion SHALL NOT silently merge, cherry-pick, or apply results to the parent. Failure or cancellation SHALL retain recoverable work and report result-capture failures explicitly.

#### Scenario: Result excludes inherited edits
- **WHEN** the parent baseline includes changes to file A and the delegate changes only file B
- **THEN** the delegate result presents the baseline-to-result change to B rather than claiming inherited changes to A as delegate work
- **AND** the parent checkout remains unchanged by completion

#### Scenario: Uncommitted delegate output
- **WHEN** a delegate finishes with uncommitted edits
- **THEN** the result is reviewable from a retained immutable result reference if capture succeeds
- **AND** the child's index and working files remain unchanged by result publication

#### Scenario: Interrupted delegate
- **WHEN** a delegate fails or is cancelled after editing files
- **THEN** its checkout is retained and reported as recoverable rather than automatically removed

#### Scenario: Dirty-baseline integration guidance
- **WHEN** a user inspects a result based on uncommitted parent edits
- **THEN** the system identifies the baseline-relative delta and warns that directly merging the private branch can also introduce baseline changes
- **AND** the system does not imply the result has already been applied or accepted
