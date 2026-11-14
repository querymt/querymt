# Spec Delta

## Purpose

Defines how a remote session's events reach the attaching agent: each event crosses the mesh once in bounded chunks, the local copy keeps the owner's order and stays consistent with it, reopening fetches only new events, and load-time requests do not wait on each other.

## ADDED Requirements

### Requirement: Chunked event transfer
The owner of a session SHALL serve its events in chunks of at most 5,000 events and at most 4 MiB of encoded data, in either direction from a given position. Each chunk SHALL contain at least one event when events remain, and SHALL report the stream tip and whether more events remain.

#### Scenario: Typical session in one request
- **WHEN** an attaching agent requests the events of a session with 3,000 events totalling 2 MiB
- **THEN** the owner returns all events in one chunk and reports that no more events remain

#### Scenario: Byte cap splits the transfer
- **WHEN** the requested events exceed 4 MiB
- **THEN** the owner returns a chunk of at most 4 MiB and reports that more events remain

#### Scenario: Single oversized event
- **WHEN** the next event alone exceeds 4 MiB
- **THEN** the owner returns a chunk containing that event, so the transfer still advances

### Requirement: Each remote event transferred once
When a remote session is loaded or attached, including the client connection subscribing to it, the attaching agent SHALL transfer each event from the owner at most once and SHALL build every response from its local copy of the session's events. An event MAY be transferred again only to repair a detected gap, after falling back from a failed chunk request, or after the owner's history was rewritten. Concurrent fetches that cover the same range SHALL NOT both transfer it.

#### Scenario: First open of a remote session
- **WHEN** a remote session with 4,000 events is opened for the first time, through session load or through remote attach
- **THEN** each of those events crosses the mesh exactly once during the open

#### Scenario: Overlapping fetches
- **WHEN** an older-history request needs a range that the background fill is about to transfer
- **THEN** the events in that range cross the mesh once

### Requirement: Local copy follows the owner's order
Reads of a remote session's local copy SHALL order events by their position in the owner's history, not by the order in which they arrived. A read SHALL NOT present a range as complete while it contains events that have not been transferred.

#### Scenario: Recent events arrive first
- **WHEN** the recent window of a remote session was transferred before its older events
- **THEN** every load and older-history response lists events in the owner's order

#### Scenario: Interrupted background fill
- **WHEN** older history is requested for a range that an interrupted fill left incomplete
- **THEN** the attaching agent transfers the missing events before answering, and does not return the range with a gap

### Requirement: Historical events are not delivered as live updates
Events transferred to fill a remote session's history SHALL NOT be delivered to connected clients as live session updates. Events relayed live, and events recovered to repair a gap in the live relay, SHALL be delivered.

#### Scenario: Background fill while a client is connected
- **WHEN** the attaching agent fills older history while a client is connected to the session
- **THEN** the client receives no session updates for those events

### Requirement: Recent events transferred first
On the first open of a remote session with a load window, the attaching agent SHALL fetch the most recent events before older ones, and SHALL answer the load as soon as the recent window is available locally.

#### Scenario: Long remote session
- **WHEN** a remote session with 50,000 events is opened for the first time with a load window
- **THEN** the load response is returned after the recent window has been transferred, without waiting for the older events

### Requirement: Incremental transfer on reopen
When a remote session whose owner supports chunked transfer is reopened, the attaching agent SHALL transfer only events newer than those it already holds locally, and the load response SHALL include them, whether or not an existing attachment is reused.

#### Scenario: Reopening after new activity
- **WHEN** a remote session that was previously synced has gained 30 events since, and is reopened
- **THEN** only those 30 events cross the mesh

#### Scenario: Reopening without new activity
- **WHEN** a previously synced remote session has no new events and is reopened
- **THEN** no history events cross the mesh

#### Scenario: Reopening while still attached
- **WHEN** a remote session is reopened while its attachment is still active, and the owner has gained events
- **THEN** the load response includes those events

### Requirement: Rewritten owner history is detected
When the owner of a session that supports chunked transfer changes its history other than by appending, such as after an undo, the attaching agent SHALL detect the change on its next sync and SHALL replace the affected part of its local copy before answering a load. An owner without chunked transfer cannot report such a change, so for it the attaching agent SHALL instead replace its local copy with a full transfer on each sync.

#### Scenario: Undo on the owner
- **WHEN** turns are undone on the owner and the session is then reopened from another node
- **THEN** the undone turns do not appear in the load response

#### Scenario: Undo on an older owner
- **WHEN** turns are undone on an owner without chunked transfer and the session is then reopened from another node
- **THEN** the attaching agent replaces its local copy with a full paged transfer
- **AND** the undone turns do not appear in the load response

### Requirement: Older history available from the attaching agent
An older-history request for a remote session SHALL be answered from the attaching agent's local copy, transferring any part of the requested range that is not yet local.

#### Scenario: Range not yet synced
- **WHEN** older history is requested for a range of a remote session that has not been transferred yet
- **THEN** the attaching agent transfers that range from the owner and returns it, and later requests for the same range use the local copy

### Requirement: Load-time requests run concurrently
After a remote session is connected, the attaching agent SHALL obtain the session snapshot, runtime status, profile and configuration options concurrently. A failure or timeout of the runtime status, profile or configuration request SHALL NOT fail the load.

#### Scenario: Slow profile lookup
- **WHEN** the profile lookup takes 3 seconds and the other requests take 200 ms
- **THEN** the load completes in about 3 seconds, not the sum of all request times

#### Scenario: Configuration unavailable
- **WHEN** the configuration options request fails
- **THEN** the load still succeeds without configuration options

#### Scenario: Runtime status unavailable
- **WHEN** the runtime status request fails
- **THEN** the load still succeeds without runtime status

### Requirement: Older owners remain supported
When the owner does not support chunked transfer, the attaching agent SHALL fall back to the existing paged transfer and the load SHALL still succeed with the same content. The attaching agent SHALL remember that the owner lacks support, so later transfers do not repeat the failed request. Incremental transfer, tail-first transfer and epoch-based rewrite detection are not available for such an owner; each sync replaces the local copy with a full paged transfer.

#### Scenario: Owner on an older version
- **WHEN** a remote session is opened from an owner that rejects chunked requests as unknown
- **THEN** the attaching agent transfers the events with paged requests and the load succeeds

#### Scenario: Second open from the same older owner
- **WHEN** another session from the same older owner is opened
- **THEN** the attaching agent uses paged requests without first sending a chunked request

### Requirement: Incomplete transfer is reported, not hidden
If the attaching agent cannot transfer the events required for a load within the load's transfer budget, the load SHALL fail with a typed error identifying the remote transfer and marked retryable, and SHALL NOT return a response that omits events within the requested window.

#### Scenario: Owner becomes unreachable mid-transfer
- **WHEN** the owner stops responding before the recent window has been transferred
- **THEN** the load fails with a remote-transfer error instead of returning partial history, and the error is marked retryable

#### Scenario: Owner offline, window already local
- **WHEN** the owner is unreachable and the requested window was already transferred in full
- **THEN** the load succeeds from the local copy and is marked as possibly out of date
