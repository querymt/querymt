# Spec Delta

## ADDED Requirements

### Requirement: Managed workspace snapshots reuse retained baseline content
The snapshot store of a managed workspace checkout SHALL read objects from its source repository for as long as the workspace's baseline is retained there. A snapshot SHALL NOT write an object that is reachable from that retained baseline, unless the object is a file of at least the large-file threshold. The store SHALL stay a plain git object store readable by `git`.

#### Scenario: Fresh delegate workspace snapshot writes no baseline content
- **WHEN** the first snapshot is taken in a newly provisioned isolated delegate checkout whose files equal its baseline and are all smaller than the large-file threshold
- **THEN** the snapshot records every in-scope file
- **AND** it writes no blob object to the snapshot store

#### Scenario: Large baseline file is stored in the snapshot store
- **WHEN** the first snapshot is taken in a newly provisioned delegate checkout whose baseline contains a file of at least the large-file threshold
- **THEN** that file's blob is written to the snapshot store itself
- **AND** restoring it does not hold its whole content in memory

#### Scenario: Undo after the source branch moves
- **WHEN** the source checkout's branch is reset, its unreachable objects are pruned, and the delegate then undoes a turn of its workspace
- **THEN** the undo restores the baseline contents, because the workspace's retained baseline keeps them in the source repository

### Requirement: Managed workspace snapshots store all other content themselves
Every object a snapshot of a managed workspace records that is not reachable from the workspace's retained baseline SHALL be written to the snapshot store itself, including content the workspace has staged or committed in the source repository. A snapshot SHALL NOT reuse a previously recorded object id unless that object is in the snapshot store itself or reachable from the retained baseline.

#### Scenario: Content edited in the workspace is stored in the snapshot store
- **WHEN** a delegate edits a file and a snapshot is taken
- **THEN** the edited contents are written to the snapshot store itself

#### Scenario: Cached file whose object only the source holds is stored again
- **WHEN** a file's stat data matches the snapshot store's cache, its object is no longer in the snapshot store, and only the source holds it, outside the retained baseline
- **THEN** the snapshot writes that file's blob to the snapshot store itself

#### Scenario: Content committed in the workspace survives source pruning
- **WHEN** a delegate commits a new file in its workspace, a snapshot is taken, and the workspace branch is later deleted and the source's unreachable objects are pruned
- **THEN** restoring that snapshot restores the committed file's contents
- **AND** the snapshot store passes `git fsck --connectivity-only`

### Requirement: Other snapshot stores stay self-contained
Snapshots of checkouts that are not managed workspaces, and of managed workspaces whose source repository uses a different object format, SHALL NOT read another repository's objects.

#### Scenario: Unmanaged checkout keeps a self-contained store
- **WHEN** a snapshot is taken in a session whose working directory is not a managed workspace
- **THEN** its snapshot store reads no other repository's objects

#### Scenario: Source repository with a different object format
- **WHEN** a managed workspace's source repository uses an object hash format other than the snapshot store's
- **THEN** snapshots of that workspace work as for an unmanaged checkout and write the content they capture
