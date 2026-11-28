# Spec Delta

## ADDED Requirements

### Requirement: Baseline capture reads only changed content
Baseline capture SHALL NOT read the content of a tracked file whose stat data matches its entry in the source's real index, as Git compares stat data. It SHALL take that entry's content identity instead. It SHALL read modified tracked files and non-ignored untracked files.

#### Scenario: Clean checkout reads no file content
- **WHEN** an isolated delegate is requested from a checkout whose working files all match the index and HEAD, and the index was written in a later timestamp tick than any of them was modified
- **THEN** the capture reads no file content
- **AND** the baseline identifies HEAD's commit

#### Scenario: One edited file is the only file read
- **WHEN** an isolated delegate is requested after one tracked file was edited in a checkout of many files
- **THEN** the capture reads that file and no unchanged tracked file
- **AND** the delegate's checkout has the edited contents

#### Scenario: Staged then edited file keeps working contents
- **WHEN** a file was staged and then edited again before an isolated delegate is requested
- **THEN** the baseline holds the edited working contents, not the staged contents
- **AND** the real index is byte-for-byte unchanged

### Requirement: Index entries that can hide working content are read
Baseline capture SHALL read a tracked file whose real-index entry is racily clean (modified no earlier than the index was written) or carries an assume-unchanged, skip-worktree or fsmonitor-valid flag, even when its stat data matches the entry.

#### Scenario: Racily clean entry is read
- **WHEN** a tracked file's index entry is racily clean and the file was rewritten with the same size and modification time
- **THEN** the capture reads that file
- **AND** the baseline holds its current contents

#### Scenario: Assume-unchanged file edited is captured
- **WHEN** a tracked file marked assume-unchanged in the real index was edited before an isolated delegate is requested
- **THEN** the capture reads that file
- **AND** the baseline holds its edited contents

### Requirement: Baselines hold raw working contents
The baseline SHALL hold the raw working bytes and on-disk modes of the checkout, and SHALL equal one built by reading every candidate file. Baseline capture SHALL read a tracked file whose effective attributes or configuration convert its content between the index and the working tree.

#### Scenario: Line-ending converted file keeps working bytes
- **WHEN** a tracked file's attributes convert line endings between the index and the working tree
- **THEN** the capture reads that file
- **AND** the baseline holds its working bytes, as a capture that reads every file would

#### Scenario: Selective capture matches a full read
- **WHEN** a checkout holds a tracked deletion, an edited file, a staged-only addition, an executable, a symlink and a non-ignored untracked file
- **THEN** the baseline tree is identical to the tree obtained by reading every candidate file

### Requirement: Baseline capture writes no content the repository holds
Baseline capture SHALL NOT write an object to the source repository that the repository already holds. It SHALL write only the blobs, trees and commit of contents the repository does not yet contain.

#### Scenario: Repeated delegation from the same dirty state
- **WHEN** two isolated delegates are requested one after the other from a checkout whose working state did not change in between
- **THEN** the second capture writes no new object

#### Scenario: New untracked file is written once
- **WHEN** an isolated delegate is requested after a new non-ignored file was created
- **THEN** the capture writes that file's blob
- **AND** the baseline includes the file

### Requirement: Capture validation reads file contents once per attempt
Each capture attempt SHALL read the content of each file at most once, except that a captured file modified within the racy window of the attempt's start SHALL be read once more to verify the captured content. A verified file whose content no longer matches SHALL be treated as a change detected during capture, which is retried or fails as already required.

#### Scenario: Same-size rewrite after the read is detected
- **WHEN** a captured file is rewritten with the same size after the capture read it, and its stat data after the rewrite equals its stat data before the capture
- **THEN** the attempt is discarded and a new validated capture is made, or the delegation fails before the child launches

#### Scenario: Quiet checkout needs one attempt
- **WHEN** nothing changes the checkout while the capture runs
- **THEN** the baseline is produced by one attempt that read each changed file once, apart from files verified because they were modified within the racy window

### Requirement: Capture validation compares stat fingerprints
Validation SHALL compare HEAD, the index entries, and the size, modification time and mode of every captured path and every non-ignored untracked path, plus change time and inode where the platform provides them and the repository's stat settings use them, taken before and after the capture. Any difference SHALL be treated as a change detected during capture, which is retried or fails as already required.

#### Scenario: External edit to an already dirty file during capture
- **WHEN** a file that already differs from HEAD is rewritten by another process while the capture runs
- **THEN** the attempt is discarded and a new validated capture is made, or the delegation fails before the child launches

#### Scenario: Untracked file created during capture
- **WHEN** a new non-ignored file appears in the checkout while the capture runs
- **THEN** the attempt is discarded and a new validated capture is made, or the delegation fails before the child launches

### Requirement: Isolated delegation startup is traceable
When tracing is enabled, each phase between claiming a delegation and sending the delegate's first prompt SHALL be recorded as its own span under the delegation's handler span. The parallel-slot wait and `delegation.execute` SHALL be siblings. Admission, parent workspace resolution, baseline capture, lock and gate waits, provisioning, the `delegation_start` hook and session creation SHALL be under `delegation.execute`.

#### Scenario: No unattributed startup time
- **WHEN** an isolated delegation starts and its delegate receives its first prompt
- **THEN** the spans under the handler span cover the interval from the end of the claim to the first prompt, except for gaps of at most 50 ms in total

#### Scenario: Queued delegation shows its wait
- **WHEN** a delegation waits because the parallel-delegation limit is reached
- **THEN** a queue-wait span records the time from the claim to the start of execution

#### Scenario: Shared delegation has no allocation spans
- **WHEN** a shared (non-isolated) delegation starts
- **THEN** no baseline capture or provisioning span is recorded for it

### Requirement: Startup spans record what they did
A baseline capture span SHALL record its attempt count, files read, bytes read and objects written. A lock or gate wait span SHALL cover only the time spent waiting and SHALL record the lock it waited for, and a gate wait span SHALL record the checkout the gate serialises. A provisioning span SHALL record the number of files checked out. The workspace id SHALL be recorded once it exists.

#### Scenario: Capture span explains a slow capture
- **WHEN** a capture reads 100 files totalling 10 MB in one attempt
- **THEN** its span records 1 attempt, 100 files read and 10 MB read

#### Scenario: Contended gate is visible
- **WHEN** baseline capture waits for the source checkout's mutation gate held by another operation
- **THEN** a gate-wait span records the wait time and the checkout it serialises
