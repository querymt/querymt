# session-storage-reliability Specification

## Purpose

Keeps the local session store usable after an individual storage operation fails, keeps event stream setup from stalling async execution, and preserves the cause of storage failures for callers.

## Requirements

### Requirement: A failed storage operation does not disable the store
When one session storage operation fails, including by panicking, QueryMT SHALL report an error for that operation only. Later storage operations on the same store SHALL run normally and SHALL NOT fail because of the earlier failure.

#### Scenario: Storage keeps working after an operation panics
- **WHEN** a storage operation panics while it holds the session database connection
- **THEN** that operation returns an error to its caller
- **AND** a following session load, event append and session list each succeed

#### Scenario: Storage keeps working after an operation returns an error
- **WHEN** a storage operation fails with a database error
- **THEN** the next storage operation on the same store succeeds

### Requirement: Opening an event stream does not block async execution
Creating a session's event stream SHALL NOT wait on the session database connection on an async runtime worker thread. Event numbering for a newly opened stream SHALL continue after the highest sequence number already stored for that session.

#### Scenario: Stream opens while the connection is busy
- **WHEN** a long storage operation holds the database connection and another task opens a session event stream on a single-worker async runtime
- **THEN** other async tasks on that runtime keep making progress while the stream is opened

#### Scenario: Reopened stream continues numbering
- **WHEN** a session has stored events up to sequence number N and its event stream is opened in a new process
- **THEN** the first event emitted on that stream is numbered N + 1

### Requirement: Storage failures keep their cause
A storage failure reported to callers SHALL identify its kind: not found, database error, serialization error, or an aborted or panicked storage task. The original error SHALL stay available as the error's source. A storage task that panicked SHALL be reported as a task failure, not as a generic message.

#### Scenario: Panicked storage task is reported as a task failure
- **WHEN** a storage operation panics
- **THEN** the caller receives a task-failure error whose source is the task's join error

#### Scenario: Storage error converted for a provider caller keeps its source
- **WHEN** a session storage error is converted into an LLM error for a provider-facing caller
- **THEN** the converted LLM error's message includes the message of every error in the storage error's source chain, including the original database error

### Requirement: Delegation claims retry only transient storage errors
When claiming a delegation fails because the database is busy or locked, QueryMT SHALL retry up to three attempts in total. Any other claim failure SHALL be returned after the first attempt, with its storage error kind intact.

#### Scenario: Busy database claim is retried
- **WHEN** the first claim attempt fails because the database is busy and the second succeeds
- **THEN** the delegation is claimed

#### Scenario: Non-transient claim failure is not retried
- **WHEN** a claim attempt fails because the stored delegation status cannot be decoded
- **THEN** exactly one claim attempt is made and a database error is returned

#### Scenario: Missing delegation is a claim outcome, not a failure
- **WHEN** a delegation that does not exist is claimed
- **THEN** exactly one claim attempt is made and the result reports the delegation as not found
