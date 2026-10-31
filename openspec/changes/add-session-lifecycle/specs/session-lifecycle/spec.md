# Spec Delta

## Purpose

Let users resolve or archive sessions, automatically resolve terminal one-shot delegation attempts, and retrieve matching sessions without deleting history or changing delegation outcomes.

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

### Requirement: Resolve terminal one-shot delegation attempts
QueryMT SHALL resolve a created one-shot delegation child after the first successful expected-state transition to `Complete`, `Failed`, or `Cancelled`. This marks the attempt finished, not successful. Only the child's resolved state changes; its archive state and other sessions remain unchanged. The update SHALL use the target handle owning the child. Pre-creation failure, running state, and transient interruption SHALL NOT resolve it.

#### Scenario: Delegation finishes successfully
- **WHEN** a delegation's first expected-state transition to `Complete` succeeds after execution and configured verification
- **THEN** its child session is persisted as resolved
- **AND** its final result is made available to the planner unchanged
- **AND** its archived state and the planner session's lifecycle state remain unchanged

#### Scenario: Execution or verification fails
- **WHEN** a delegation's first expected-state transition to `Failed` succeeds, including after a verification failure
- **THEN** its existing failure status, error, and result exposure remain unchanged
- **AND** its created child session is resolved because the attempt is terminal, not because it succeeded
- **AND** its archived state and the planner session's lifecycle state remain unchanged

#### Scenario: Failure occurs before child creation
- **WHEN** setup fails before a child session is created
- **THEN** no session lifecycle state is changed

#### Scenario: Delegation is terminally cancelled
- **WHEN** cancellation successfully transitions a running delegation to `Cancelled`
- **THEN** its created child session is resolved
- **AND** a merely running or transiently interrupted attempt remains unchanged

#### Scenario: Reopen and replay a terminal attempt
- **WHEN** a user explicitly reopens a completed, failed, or cancelled delegated child
- **AND** the terminal delegation event is replayed
- **THEN** the child remains open because the expected-state transition can succeed only once

#### Scenario: Start a new attempt after failure
- **WHEN** an agent chooses to retry by creating another delegation
- **THEN** the new child starts open and unarchived
- **AND** the new attempt is not restricted by the prior attempt's terminal state

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

### Requirement: Preserve delegation outcomes on lifecycle write failure
An automatic lifecycle write failure or unsupported owner SHALL be logged with delegation and child context. It SHALL NOT alter the terminal delegation status, existing result or error exposure, or execution outcome, and SHALL NOT claim the child was persisted as resolved.

#### Scenario: Lifecycle persistence is unavailable
- **WHEN** the terminal delegation transition succeeds but the child's lifecycle write fails or is unsupported
- **THEN** the delegation's existing result or error is still returned or exposed unchanged
- **AND** the lifecycle write failure is logged without claiming the child was persisted as resolved

### Requirement: Execution and history are unaffected
Lifecycle state SHALL NOT change loading, prompting, cancellation, tasks, schedules, history, deletion semantics, or conversation activity timestamps. The first successful terminal status transition of a created one-shot delegation SHALL be the only automatic lifecycle transition; ordinary turns and non-terminal interruptions SHALL NOT trigger it.

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
