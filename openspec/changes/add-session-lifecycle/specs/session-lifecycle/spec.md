# Spec Delta

## Purpose

Let users resolve or archive sessions, automatically resolve completed one-shot delegations, and retrieve matching sessions without deleting history or changing agent execution.

## ADDED Requirements

### Requirement: Persistent independent states
QueryMT SHALL persist independent resolved and archived states per session. New and existing sessions without explicit state SHALL be open and unarchived. A change SHALL apply only to the requested session, not its parents or children.

#### Scenario: Default and retained state
- **WHEN** a new session is created or an existing session has no lifecycle metadata
- **THEN** it is open and unarchived
- **AND** persisted states survive backend restart

#### Scenario: Independent sessions
- **WHEN** a session is resolved or archived
- **THEN** its parents and children retain their own states
- **AND** a subsequently created fork starts open and unarchived

### Requirement: Reversible explicit updates
QueryMT SHALL allow resolve, reopen, archive, and restore through explicit desired-state updates. Omitted fields SHALL remain unchanged. Updates SHALL be atomic and idempotent, work without loading the conversation, and return persisted state. Invalid input or an unknown session SHALL fail without changing state.

#### Scenario: Archive and restore
- **WHEN** an open or resolved session is archived and later restored
- **THEN** its resolved state remains unchanged throughout

#### Scenario: Reopen and repeated updates
- **WHEN** a resolved session is reopened
- **THEN** it becomes open without changing its archived state
- **AND** repeating the same desired-state update has no additional effect

#### Scenario: Rejected update
- **WHEN** an update contains invalid values or references an unknown session
- **THEN** the backend returns an error and persists no partial change

### Requirement: Resolve completed delegated sessions
QueryMT SHALL automatically resolve a delegated child session when its one-shot delegation successfully completes and its final result is made available to the planner. This SHALL preserve its archived state and all other sessions' states. Failed, cancelled, or interrupted delegations SHALL NOT trigger automatic resolution. Ordinary turn completion and user-created forks SHALL NOT trigger it.

#### Scenario: Delegation finishes successfully
- **WHEN** a delegation successfully completes, including configured verification, and returns its final result
- **THEN** its child session is persisted as resolved as part of completion handling
- **AND** subsequent listings report it as resolved without requiring a client action
- **AND** its archived state and the planner session's lifecycle state remain unchanged

#### Scenario: Delegation does not complete successfully
- **WHEN** a delegation fails, is cancelled, is interrupted, or fails verification
- **THEN** its lifecycle state is not automatically changed

#### Scenario: Reopen a completed delegate
- **WHEN** a user explicitly reopens a completed delegated session
- **THEN** it remains open when loaded or listed again
- **AND** replaying its earlier completion does not resolve it again

### Requirement: Optional lifecycle filters
Session listing and search SHALL accept independent resolution filters (`any`, `open`, `resolved`) and archive filters (`any`, `excluded`, `only`). Omitted filters SHALL mean `any`, preserving current visibility. Unknown filter values SHALL be rejected rather than silently ignored.

#### Scenario: Select a working set
- **WHEN** listing with resolution `open` and archive `excluded`
- **THEN** only open, unarchived sessions matching the other supplied filters are returned

#### Scenario: Find unresolved sessions
- **WHEN** resolution `open` is requested and the archive filter is omitted
- **THEN** only unresolved sessions are returned, including archived ones, subject to the other supplied filters
- **AND** adding archive `excluded` restricts the results to unresolved, unarchived sessions

#### Scenario: Resolved, archived, and unfiltered results
- **WHEN** resolution `resolved` or archive `only` is requested
- **THEN** results satisfy each supplied condition
- **AND** omitting both filters returns all lifecycle states under the existing listing rules

### Requirement: Filtering before pagination
Lifecycle filters SHALL compose with existing workspace, relationship, and text filters before pagination. Returned rows, group previews, group activity, totals, and continuation cursors SHALL reflect the same filtered result. Existing relationship scopes SHALL retain their meanings.

#### Scenario: Matches beyond the first unfiltered page
- **WHEN** matching sessions occur after nonmatching sessions in the unfiltered ordering
- **THEN** the filtered first page includes those matches up to its limit
- **AND** counts and subsequent pages describe matching sessions, not the unfiltered set

### Requirement: Execution and history are unaffected
Lifecycle state SHALL NOT change loading, prompting, cancellation, tasks, schedules, history, deletion semantics, or conversation activity timestamps. Successful one-shot delegation completion SHALL be the only automatic lifecycle transition; all other changes SHALL require an explicit update.

#### Scenario: Continue a hidden session
- **WHEN** a resolved or archived session is loaded or receives another prompt
- **THEN** its conversation remains accessible and normal execution remains possible
- **AND** its lifecycle state remains unchanged until explicitly updated

### Requirement: Lifecycle metadata in listings
A backend supporting lifecycle SHALL include current lifecycle metadata for every listed session whose state it can authoritatively determine, whether or not lifecycle filters are supplied. Metadata SHALL include resolved and archived flags and their nullable timestamps. Unavailable metadata SHALL NOT be replaced with assumed open/unarchived values.

#### Scenario: Unfiltered listing includes state
- **WHEN** a supported backend lists sessions without lifecycle filters
- **THEN** each session with authoritative state includes its lifecycle metadata
- **AND** a local session without a lifecycle record reports both flags as false and both timestamps as null

#### Scenario: Remote state is unavailable
- **WHEN** an unfiltered listing falls back to a remote bookmark without authoritative lifecycle metadata
- **THEN** lifecycle metadata is absent rather than fabricated
- **AND** a lifecycle-filtered request still follows the remote-owner error requirement

### Requirement: Additive backend contract
QueryMT SHALL advertise lifecycle support and expose persisted state, updates, and filters through a shared backend contract. Existing clients and storage integrations SHALL continue working without adopting it. An unsupported backend SHALL report unsupported operations rather than claim success or silently ignore a requested lifecycle filter.

#### Scenario: Read after updating
- **WHEN** a supported update succeeds
- **THEN** a subsequent lifecycle read or session listing reports the persisted state
- **AND** clients that omit the new options retain existing behavior

### Requirement: Remote owner authority
Remote lifecycle reads, writes, and filtering SHALL use the authoritative session owner. Filtered remote pages SHALL be filtered at that owner before pagination. Unavailable or unsupported owners SHALL produce an explicit error for lifecycle operations, not a local substitute state or an unfiltered fallback.

#### Scenario: Remote update and unavailable owner
- **WHEN** a remote lifecycle update succeeds
- **THEN** subsequent reads from its owner return that state
- **AND** an unavailable or unsupported owner cannot be reported as a successful update or a complete filtered result

### Requirement: Consistent support across clients
All QueryMT clients SHALL support reading lifecycle state, resolve/reopen, archive/restore, and lifecycle filtering through the same backend contract. Clients SHALL distinguish persisted lifecycle resolution from ordinary runtime completion and handle unavailable capability explicitly. Client-specific implementation is separate from this backend change.

#### Scenario: Switch clients
- **WHEN** one client updates a session and another client fetches it
- **THEN** both interpret the same persisted lifecycle state and filter semantics
- **AND** no client-local state replaces the backend as the source of truth
