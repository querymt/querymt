# Spec Delta

## Purpose

Defines when the end-of-run duplicate-code review (`[[middleware]]` with `type = "dedup_check"`) runs, which file changes it reviews and how it continues a run when it finds duplicates.

## ADDED Requirements

### Requirement: Each run that changed files is reviewed once
When a run is about to complete and its tool calls added or modified files, the duplicate-code review SHALL run once for that run. A run that changed no files SHALL NOT be reviewed. The review SHALL run in every such run of every session, not only in the first one of a process.

#### Scenario: Second run of a session is reviewed
- **WHEN** a session's first run changed files and was reviewed, and its second run also changes files
- **THEN** the second run is reviewed before it completes

#### Scenario: Run without file changes is not reviewed
- **WHEN** a run completes without any tool call that added or modified a file
- **THEN** no review runs and no `duplicate_code` notice is stored

#### Scenario: Review runs once per run
- **WHEN** a review found duplicates and continued the run, and the model then ends its turn again in the same run
- **THEN** no second review runs in that run

### Requirement: A review covers the changes of its own run
The review SHALL consider exactly the files that tool calls of the reviewed run added or modified. Changes made by an earlier run of the session, or by a run of another session, SHALL NOT be included. Changes that were not reviewed when their run ended SHALL be dropped, not carried into a later run.

#### Scenario: Changes of an earlier unreviewed run are not included
- **WHEN** a run changed file A while the review was unavailable, and the session's next run changes only file B
- **THEN** the next run's review considers file B and not file A

#### Scenario: Sessions' changes are reviewed separately
- **WHEN** two sessions of one agent run at the same time and each changes a different file
- **THEN** each run's review considers only the file its own session changed

#### Scenario: Changes made after the review are dropped
- **WHEN** a review continued a run and the model changes another file before ending its turn
- **THEN** that later change is not reviewed in this run and is not included in the next run's review

### Requirement: A review continues its own run
When a review finds duplicates above the configured threshold, it SHALL continue the reviewed run with a `duplicate_code` notice appended to that run's conversation. The next model request SHALL contain the reviewed session's history followed by the notice, and nothing from another session.

#### Scenario: Duplicates found
- **WHEN** a run added a function that duplicates an indexed function above the threshold
- **THEN** a `duplicate_code` notice is stored in that session and another model request is sent in the same run

#### Scenario: Concurrent sessions each continue with their own conversation
- **WHEN** two sessions of one agent run at the same time and the review finds duplicates in one of them
- **THEN** the continuation request of that run contains only that session's messages, and the other session's run is not continued

#### Scenario: No duplicates found
- **WHEN** a run changed files and the review finds no duplicate above the threshold
- **THEN** the run completes without a `duplicate_code` notice

### Requirement: An unavailable review does not block a run
When the review cannot run, because it is disabled or the workspace index is not ready, the run SHALL complete as if no review were configured.

#### Scenario: Workspace index not ready
- **WHEN** a run changed files and the workspace index is still being built when the run is about to complete
- **THEN** the run completes without a review and without an error

#### Scenario: Review disabled
- **WHEN** the review is configured with `enabled = false` and a run changes files
- **THEN** the run completes without a review
