# Spec Delta

## Purpose

Defines how a local session run's start and end are recorded durably, how a single live owner of a session's runs is established across processes on one machine, and how runs abandoned by a process that died are detected and closed.

## ADDED Requirements

### Requirement: Every recorded run start is eventually closed
QueryMT SHALL record exactly one durable run-completion event for every durable run-start event of a local session. A run that ends normally, fails or is cancelled is closed by its owner. A run whose owner died SHALL be closed by reconciliation with the outcome `interrupted`.

#### Scenario: Normal run is closed by its owner
- **WHEN** a local run completes, fails or is cancelled while its process is alive
- **THEN** the journal contains one run-completion event for that run ID with outcome `completed`, `failed` or `cancelled`

#### Scenario: Crashed run is closed as interrupted
- **WHEN** a process dies while a run is in progress and the session is later loaded by a live process
- **THEN** the journal gains one run-completion event for that run ID with outcome `interrupted`

### Requirement: Exclusive cross-process run ownership
While a local run is in progress, its process SHALL hold an exclusive run lease for that session that other processes on the same machine can observe. The operating system SHALL release the lease when the owning process exits for any reason, including abnormal termination. The lease SHALL be acquired before the run-start event is recorded and released only after the run-completion event is recorded.

#### Scenario: Lease released on crash
- **WHEN** a process holding a session's run lease is killed without cleanup
- **THEN** another process can acquire that session's run lease immediately afterwards

#### Scenario: Lease held during a run
- **WHEN** a run is in progress in one process
- **THEN** a second process on the same machine observes the session's run lease as held

#### Scenario: Lease is kept across queued turns
- **WHEN** a run completes while another prompt of the same session is queued in that process
- **THEN** the queued run starts under the same lease without releasing it in between
- **AND** no other process can start a run of the session between the two turns

### Requirement: Run lease behaves identically on Windows, Linux and macOS
The run lease SHALL provide the same exclusivity and crash-release guarantees on Windows, Linux and macOS. Lease storage SHALL be valid on all three platforms. A lease file that persists after its run SHALL NOT cause the lease to be treated as held.

#### Scenario: Leftover lease file does not block a run
- **WHEN** a session's lease file exists from an earlier run but no process holds it
- **THEN** a new run acquires the lease and starts normally on every supported platform

#### Scenario: Crash release on Windows
- **WHEN** on Windows a process holding a run lease is terminated
- **THEN** another process acquires the lease and reconciles the orphaned run

### Requirement: A live run in another process is never closed
QueryMT SHALL NOT record a run-completion event for a run whose session lease is held by any live process, including the reconciling process itself.

#### Scenario: Loading a session running elsewhere
- **WHEN** process B loads a session whose run is in progress in process A
- **THEN** process B records no run-completion event for that run
- **AND** the run is closed by process A when it ends

### Requirement: Reconciliation points
QueryMT SHALL reconcile a local session's orphaned run before its actor accepts input, before a session load snapshot is served, and when Cancel or Stop is requested while that process has no in-memory run for the session.

#### Scenario: Loading a crashed session clears its running state
- **WHEN** a client loads a local session whose last run start has no completion and whose lease is free
- **THEN** the served snapshot already contains the `interrupted` run-completion event
- **AND** a client deriving run state from run lifecycle events shows the session as not running

#### Scenario: Cancel on a crashed session
- **WHEN** Cancel is requested for a session with no in-memory run whose last run start has no completion and whose lease is free
- **THEN** QueryMT records an `interrupted` run-completion event for that run

#### Scenario: Stop on an unloaded session
- **WHEN** Stop is requested for a local session that is not loaded and has an orphaned run
- **THEN** QueryMT records an `interrupted` run-completion event for that run

### Requirement: Reconciliation is idempotent
Reconciling a session SHALL record at most one run-completion event per run ID, regardless of how many times or from how many processes reconciliation runs. A session with no unfinished run SHALL be left unchanged.

#### Scenario: Repeated loads
- **WHEN** a session with an orphaned run is loaded twice
- **THEN** the journal contains exactly one `interrupted` run-completion event for that run

#### Scenario: Concurrent reconcilers
- **WHEN** two processes reconcile the same orphaned session at the same time
- **THEN** exactly one `interrupted` run-completion event is recorded

#### Scenario: Several orphaned runs
- **WHEN** a session's journal holds more than one run start without a completion and its lease is free
- **THEN** a single reconciliation records one `interrupted` run-completion event for each of them

### Requirement: Runs refuse to start while another process owns the session
QueryMT SHALL reject a new run of a local session while another process holds that session's run lease. It SHALL return an error that identifies the session as running in another process, and SHALL record no run-start event. A lease held only momentarily (by reconciliation or session delete) SHALL delay the run start by a short bounded grace period instead of rejecting it.

#### Scenario: Prompt sent to a session busy in another process
- **WHEN** a prompt is submitted to a session whose run lease is held by another process
- **THEN** the prompt fails with a "running in another process" error
- **AND** no run-start event is recorded

#### Scenario: Prompt racing a reconciler
- **WHEN** a prompt is submitted while reconciliation briefly holds the session's run lease
- **THEN** the run starts once the lease is released, without a "running in another process" error

### Requirement: Remote-hosted sessions are not reconciled locally
QueryMT SHALL NOT reconcile, or record run-completion events for, a session hosted on a remote node. Run closure for such sessions belongs to the host. Run lifecycle events mirrored from a remote host SHALL NOT be treated as orphaned runs of the local node, whether or not the session is still bookmarked.

#### Scenario: Mirrored remote run after the bookmark is removed
- **WHEN** a remote session is dismissed (its bookmark removed) while its journal still holds a run start mirrored from the host with no completion
- **THEN** loading, cancelling or stopping that session records no run-completion event for that run

#### Scenario: Loading a bookmarked remote session
- **WHEN** a client loads a remote-hosted session whose relayed journal shows an unfinished run
- **THEN** the attached node records no run-completion event for it

### Requirement: Interrupted outcome is reported to clients
The run-completion event SHALL carry the outcome `interrupted` for reconciled runs, alongside the existing `completed`, `failed` and `cancelled` outcomes. The event SHALL be delivered live and on replay like any other run-completion event.

#### Scenario: Live client observes reconciliation
- **WHEN** a connected client is attached to a session while its orphaned run is reconciled
- **THEN** the client receives a run-completion update with outcome `interrupted`
