# Spec Delta

## Purpose

Defines what undo snapshots capture from a session's workspace, how undo and redo restore it, what capturing costs, and how long snapshots are kept, so that undo never reports success for content it did not save and never loses history unless the user opts into garbage collection.

## ADDED Requirements

### Requirement: Every file in scope is captured regardless of size
A snapshot SHALL capture the full content of every regular file inside the snapshot scope, whatever its size. No size limit SHALL leave a file out. The key `[agent.execution.snapshot] max_file_bytes` (and the same key under a planner's or delegate's `execution.snapshot`) SHALL NOT exist; setting it MUST fail the config load with an unknown-field error.

#### Scenario: Large file is captured and undone
- **WHEN** a turn modifies a 17 MiB file inside the workspace and the user undoes that turn
- **THEN** the file has its exact pre-turn content again

#### Scenario: Undo and redo of a file that grew restore both versions
- **WHEN** a turn grows a 1 KiB file to 17 MiB, the user undoes the turn and then redoes it
- **THEN** after undo the file has its 1 KiB content
- **AND** after redo the file has its 17 MiB content and redo reports success

#### Scenario: Removed max_file_bytes key is rejected
- **WHEN** a config sets `max_file_bytes = 1024` under `[agent.execution.snapshot]`
- **THEN** loading fails with an unknown-field error naming `max_file_bytes`

### Requirement: Files that change during capture are still captured
A file whose size or content changes while a snapshot reads it SHALL NOT be left out and SHALL NOT be captured truncated. The snapshot SHALL capture a complete copy of the file taken during the snapshot. When no stable copy can be obtained because the file keeps changing across every retry, the snapshot SHALL fail with an error naming the file rather than record a partial or stale version.

#### Scenario: File that shrinks during capture is captured
- **WHEN** a file is shorter at read time than when the snapshot listed it
- **THEN** the snapshot contains the file with its full content as read

#### Scenario: File that grows during capture is captured whole
- **WHEN** a file is longer at read time than when the snapshot listed it
- **THEN** the snapshot contains the file with all of its bytes, not only the listed length

### Requirement: Snapshot scope is decided by path
A snapshot SHALL leave out exactly the paths that match `.gitignore` files, `.ignore` files, or the repository's `.git/info/exclude`, and every file or directory whose name starts with `.`. `.gitignore` SHALL apply whether or not the workspace is a git repository. Global git excludes SHALL NOT apply. Undo and redo SHALL NOT touch a path that was outside the scope when both involved snapshots were taken.

#### Scenario: Gitignored file in a git repository is not captured
- **WHEN** a git repository's `.gitignore` lists `model.bin` and a snapshot is taken
- **THEN** the snapshot does not contain `model.bin`

#### Scenario: Gitignore applies outside a git repository
- **WHEN** a workspace that is not a git repository has a `.gitignore` listing `data/` and a snapshot is taken
- **THEN** the snapshot contains no path under `data/`

#### Scenario: Ignore file excludes paths
- **WHEN** a workspace has an `.ignore` file listing `*.gguf` and a snapshot is taken
- **THEN** the snapshot contains no `.gguf` file

#### Scenario: Git info exclude is honoured
- **WHEN** a git repository's `.git/info/exclude` lists `scratch.txt` and a snapshot is taken
- **THEN** the snapshot does not contain `scratch.txt`

#### Scenario: Dot entries are not captured
- **WHEN** a workspace contains `.venv/lib/x.py`, `src/.cache/y` and `.hidden`, plus `src/lib.rs`
- **THEN** the snapshot contains only `src/lib.rs`

#### Scenario: Undo leaves ignored files alone
- **WHEN** a turn modifies a gitignored file and a captured file, and the user undoes the turn
- **THEN** the captured file is restored and the gitignored file keeps its modified content

### Requirement: Unchanged files are not read again
A snapshot SHALL NOT read the content of a file whose size, modification time, change time and inode all match the previous snapshot of the same workspace, unless the file was modified within two seconds before the previous snapshot started.

#### Scenario: Unchanged file is not read again
- **WHEN** a file last modified an hour ago is unchanged between two snapshots
- **THEN** the second snapshot reads none of its bytes

#### Scenario: Rewrite with same size and mtime is detected
- **WHEN** a file is rewritten with different content of the same length and its modification time is set back to the previous value
- **THEN** the next snapshot captures the new content and a diff against the previous snapshot reports the file

### Requirement: An unchanged workspace reuses the previous snapshot
When the captured content of a workspace equals that of its most recent snapshot, taking a snapshot SHALL return the most recent snapshot's id and SHALL NOT record a new snapshot.

#### Scenario: Unchanged workspace returns the previous snapshot id
- **WHEN** two snapshots are taken with no file change between them
- **THEN** both return the same snapshot id

#### Scenario: Changed workspace returns a new snapshot id
- **WHEN** a captured file changes between two snapshots
- **THEN** the second snapshot returns a different id

### Requirement: Large files entering snapshots are reported once
When a file of 64 MiB or more is captured and the previous snapshot of the workspace had no copy of that path of at least 64 MiB, the snapshot SHALL log one warning naming the path and its size and pointing to `.ignore`. At most 10 such warnings SHALL be logged per snapshot, followed by one summary warning with the remaining count.

#### Scenario: Large file entering snapshots is reported once
- **WHEN** a 64 MiB file is added and two snapshots are taken, the second after modifying the file
- **THEN** exactly one warning naming that file is logged across both snapshots

#### Scenario: Many large files are summarised
- **WHEN** twelve new 64 MiB files are captured in one snapshot
- **THEN** ten warnings name individual files and one warning reports 2 more files

### Requirement: Large files are cheap to store and restore
Files of 16 MiB or more SHALL be stored without compression in the snapshot store, which SHALL stay a plain git object store readable by `git`. Restoring such a file stored as a loose object (the only form the backend writes) SHALL NOT hold its whole content in memory.

#### Scenario: Large file is stored uncompressed and readable by git
- **WHEN** a 17 MiB file is captured
- **THEN** its stored object is at least 17 MiB on disk
- **AND** `git cat-file blob <id>` in the snapshot store prints the file's exact content

#### Scenario: Large file round-trips through restore
- **WHEN** a captured 17 MiB file is overwritten and then restored from the snapshot
- **THEN** the file has its captured content byte for byte

### Requirement: Undo and redo restore moved files
A diff between two snapshots SHALL report a moved file as both its old and its new path. Undoing a turn that moved a file SHALL recreate the old path and remove the new one; redoing it SHALL move it back.

#### Scenario: Diff of a move reports both paths
- **WHEN** `a.txt` is renamed to `b.txt` between two snapshots
- **THEN** the diff reports both `a.txt` and `b.txt`

#### Scenario: Undo of a moved file restores the original path
- **WHEN** a turn renames `a.txt` to `b.txt` and the user undoes the turn
- **THEN** `a.txt` exists with its original content and `b.txt` does not exist

#### Scenario: Redo of a moved file moves it back
- **WHEN** the move undone in the previous scenario is redone
- **THEN** `b.txt` exists with the content and `a.txt` does not exist

### Requirement: Restored files are written atomically with their permissions
Restoring a file SHALL replace it in one step, so a reader sees either the old or the restored content. A restored file SHALL keep its current permission bits except the executable bits, which SHALL follow the snapshot; a recreated file SHALL get mode 0644, or 0755 when executable. A failed restore SHALL leave no temporary file behind.

#### Scenario: Executable bit follows the snapshot
- **WHEN** an executable file is captured, its executable bit is removed, and it is restored
- **THEN** the restored file is executable

#### Scenario: Restore keeps a private file private
- **WHEN** a file with mode 0600 is modified and then restored from a snapshot
- **THEN** the restored file still has mode 0600

#### Scenario: Failed restore leaves no temporary files
- **WHEN** restoring a file fails because its path is now occupied by a non-empty directory
- **THEN** the restore returns an error and the parent directory contains no entry other than those present before the restore

### Requirement: Restore failures fail undo and redo
Any failure to write, create or delete a restored path SHALL fail the undo, redo or full restore with an error naming the path. An undo that fails while restoring SHALL still record a redo frame for the workspace state before the undo. A redo that fails SHALL keep its frame.

#### Scenario: Undo that fails while restoring can be redone
- **WHEN** an undo restores two files and writing the second fails
- **THEN** the undo returns an error naming the second file
- **AND** after the cause is fixed, redo returns both files to their content before the undo

#### Scenario: Redo that fails keeps its frame
- **WHEN** a redo cannot write a restored file
- **THEN** the redo returns an error and a later redo after the cause is fixed restores the file

#### Scenario: Failed delete fails the restore
- **WHEN** undoing a turn must delete a file that the process cannot delete
- **THEN** the undo returns an error naming that file

#### Scenario: Full restore reports a write failure
- **WHEN** a full restore of a snapshot cannot write one of its files
- **THEN** the restore returns an error naming that file

### Requirement: Each turn's change record names its pre-turn snapshot
The record a turn stores for the files it changed SHALL name the snapshot taken before the turn's first change, and the turn's start record and change record SHALL carry the same turn id. Undo SHALL restore every file a later turn changed from the snapshot its change record names, whether or not the turn's start record exists.

#### Scenario: Undo of a prompt that deleted a file restores it
- **WHEN** a prompt runs a turn that deletes `notes.txt` and the user undoes to that prompt
- **THEN** `notes.txt` exists with its content from before the prompt
- **AND** undo reports `notes.txt` among the reverted files

#### Scenario: Undo works without the turn's start record
- **WHEN** a turn changed `a.txt`, its change record names the pre-turn snapshot, and no start record for the turn exists
- **THEN** undoing to the message before the turn restores `a.txt` to its pre-turn content

### Requirement: Turns recorded without a pre-turn reference stay undoable
For a change record that does not name its pre-turn snapshot, undo SHALL use the snapshot of the start record with the same turn id. When no start record has the same turn id, undo SHALL use the nearest earlier start record in the same session, provided no other change record lies between the two and no other change record has the same turn id as that start record.

#### Scenario: Earlier-format turn is undone through its start record
- **WHEN** a turn's change record names no pre-turn snapshot and a start record with the same turn id exists, and the user undoes to the message before the turn
- **THEN** the turn's changed files get their content from that start record's snapshot

#### Scenario: Turn whose records disagree on turn id is undone by position
- **WHEN** a start record with turn id `A` is followed by a change record with turn id `B` that names no pre-turn snapshot, no start record has turn id `B`, and the user undoes to the message before the turn
- **THEN** the turn's changed files get their content from start record `A`'s snapshot

#### Scenario: Overlapping turns are paired by turn id
- **WHEN** start records for turns `X` and `Y` are followed by the change record of `X` (which changed `a.txt`) and then of `Y` (which changed `b.txt`), none naming a pre-turn snapshot, and the user undoes to the message before both turns
- **THEN** `a.txt` gets its content from `X`'s start snapshot and `b.txt` from `Y`'s start snapshot

### Requirement: Undo fails before changing files when it cannot revert a whole turn
Undo SHALL fail before any workspace file changes, and SHALL record no revert frame, when a turn after the target message changed files and either no pre-turn snapshot can be found for it, or that turn's start record comes before the target message (the target lies inside a turn that may already have changed files). A target at or before the turn's start record SHALL revert the whole turn. A later turn that changed no files SHALL NOT make undo fail.

#### Scenario: Turn without a pre-turn record fails undo without changes
- **WHEN** a turn changed `a.txt`, its change record names no pre-turn snapshot, the session has no start record, and the user undoes to the message before the turn
- **THEN** undo returns an error naming the turn
- **AND** `a.txt` keeps its content and no revert frame is recorded

#### Scenario: Undo to a message inside a turn is refused
- **WHEN** a turn's start record is followed by a steering message, the turn then changes `a.txt`, and the user undoes to the steering message
- **THEN** undo returns an error saying undo reverts whole turns
- **AND** `a.txt` keeps its content and no revert frame is recorded

#### Scenario: Turn that changed no files does not block undo
- **WHEN** a later turn recorded a start record but no change record, and the user undoes to a message before it
- **THEN** undo succeeds and reports no reverted files

### Requirement: Retention settings are validated and off by default
`[agent.execution.snapshot] max_snapshots` (integer count) and `max_age_days` (integer days) SHALL default to unset, which means no garbage collection; the same keys apply under a planner's or delegate's `execution.snapshot`. When set, each MUST be at least 1; `0` or a negative value MUST fail the config load with an error naming the key. In JSON configs, `null` SHALL leave the key unset.

#### Scenario: Retention keys default to unset
- **WHEN** a config has `[agent.execution.snapshot]` with only `backend = "git"`
- **THEN** both `max_snapshots` and `max_age_days` are unset

#### Scenario: Retention values are accepted
- **WHEN** a config sets `max_snapshots = 5` and `max_age_days = 2`
- **THEN** the config loads with those values

#### Scenario: Zero retention values are rejected
- **WHEN** a config sets `max_snapshots = 0` or `max_age_days = 0`
- **THEN** loading fails with an error naming that key

#### Scenario: JSON null leaves a retention key unset
- **WHEN** a JSON config sets `"max_age_days": null` under `agent.execution.snapshot`
- **THEN** the config loads with `max_age_days` unset

#### Scenario: Configured retention reaches the snapshot store
- **WHEN** an agent is built from a config with `max_snapshots = 5` and `max_age_days = 2`
- **THEN** the garbage collection the agent runs after each turn receives a limit of 5 snapshots and 2 days

### Requirement: Without retention settings no snapshot is deleted
When neither `max_snapshots` nor `max_age_days` is set, garbage collection SHALL delete nothing and SHALL report zero removed. Undo to any recorded turn and redo of any revert frame SHALL then keep working for as long as the snapshot store exists on disk.

#### Scenario: Garbage collection without limits deletes nothing
- **WHEN** 15 snapshots exist and garbage collection runs with neither limit set
- **THEN** it reports 0 removed and 15 remaining
- **AND** every object in the snapshot store is still present

#### Scenario: Undo without limits reaches the first turn
- **WHEN** 12 turns are recorded with the default configuration, garbage collection runs after each turn, and the user undoes the first turn
- **THEN** undo succeeds and every file has its content from before the first turn

### Requirement: Retention settings prune old snapshots
When `max_snapshots` or `max_age_days` is set, garbage collection after each turn SHALL prune every snapshot beyond the `max_snapshots` most recent and every snapshot older than `max_age_days`. The most recent snapshot SHALL always be kept. The limits SHALL apply to the workspace's snapshot store, which all sessions and delegates working in that workspace share.

#### Scenario: Snapshots beyond max_snapshots are pruned
- **WHEN** 15 snapshots exist and garbage collection runs with `max_snapshots = 3`
- **THEN** it reports 12 removed and 3 remaining

#### Scenario: Snapshots older than max_age_days are pruned
- **WHEN** garbage collection runs 31 days after three snapshots were taken, with `max_age_days = 30`
- **THEN** only the most recent of them remains

#### Scenario: Latest snapshot is kept regardless of age
- **WHEN** the only snapshot is older than `max_age_days`
- **THEN** garbage collection keeps it

### Requirement: Pruned snapshot content is deleted safely
Once at least 10 snapshots are pruned, content referenced only by pruned snapshots SHALL be deleted from disk. Content referenced by a kept snapshot SHALL never be deleted, the ids of kept snapshots SHALL NOT change, and deletion SHALL NOT run while a snapshot or restore of the same workspace is in progress.

#### Scenario: Pruned snapshot content is deleted
- **WHEN** a file's only captured versions belong to 10 or more pruned snapshots and garbage collection runs
- **THEN** the store no longer contains those versions and the snapshot store's size shrinks

#### Scenario: Retained snapshot content survives pruning
- **WHEN** garbage collection deletes pruned content
- **THEN** every kept snapshot can still be fully restored by the id it was created with

#### Scenario: Garbage collection skips while a snapshot is in progress
- **WHEN** garbage collection runs while a snapshot of the same workspace is being taken
- **THEN** it deletes nothing and the snapshot completes

### Requirement: Undo and redo to pruned snapshots fail without changes
An undo or redo that needs a pruned snapshot SHALL fail before any workspace file is changed. A redo that fails this way SHALL keep its revert frame.

#### Scenario: Undo to a pruned snapshot changes no files
- **WHEN** the user undoes a turn whose pre-turn snapshot was pruned and deleted
- **THEN** undo returns an error and no workspace file changes

#### Scenario: Redo to a pruned snapshot keeps its frame
- **WHEN** the user undoes a turn, the revert frame's snapshot is then pruned and deleted, and the user redoes
- **THEN** redo returns an error, no workspace file changes, and the revert frame is still present

### Requirement: Snapshot store growth is reported
When a workspace's snapshot store on disk reaches 5 GiB, the agent SHALL log one warning naming the store's path and size and pointing to `.ignore` and to `max_snapshots`/`max_age_days`, no later than the first snapshot after another 64 MiB of file content has been captured. The warning SHALL be logged at most once per workspace per process, and SHALL NOT be logged while the store is below 5 GiB.

#### Scenario: Large snapshot store is reported once
- **WHEN** a workspace's snapshot store reaches 5 GiB and two more snapshots are taken in the same process
- **THEN** exactly one warning naming the store path and size is logged

#### Scenario: Small snapshot store is not reported
- **WHEN** a snapshot is taken and the workspace's snapshot store is below 5 GiB
- **THEN** no store-size warning is logged
