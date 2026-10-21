# Spec Delta

## Purpose

Defines the ordering, sequencing, delivery and recovery guarantees of a session's event stream, from the point where the session produces an event to every client connection attached to that session, including how the prompt response is ordered relative to the updates it produced.

## ADDED Requirements

### Requirement: Production-order delivery
QueryMT SHALL deliver a session's events to every attached connection in the order the session produced them. This SHALL hold for durable and ephemeral events, for locally hosted and remote-hosted sessions, and for events produced concurrently within one session.

#### Scenario: Back-to-back tool events stay ordered
- **WHEN** a session produces a tool-call start and the matching tool-call end within the same millisecond
- **THEN** every attached connection receives the start notification before the end notification

#### Scenario: Streaming deltas precede the stored message
- **WHEN** a session streams message deltas and then stores the complete assistant message
- **THEN** every attached connection receives all deltas of that message before the stored-message update

#### Scenario: Remote session keeps host order
- **WHEN** a remote-hosted session produces a burst of events
- **THEN** the attached node delivers them to its connections in the host's production order

### Requirement: Per-session sequence
QueryMT SHALL assign every durable event of a session a sequence number at production time that increases strictly in production order. The same number SHALL identify the event in the journal, in live delivery and on every node that relays the session.

#### Scenario: Journal order matches production order
- **WHEN** a session produces durable events A then B, however close together
- **THEN** the journal stores A with a lower session sequence than B

#### Scenario: Relayed events keep the host sequence
- **WHEN** an attached node relays a durable event of a remote-hosted session
- **THEN** the event carries the host's session sequence on the attached node

### Requirement: Replay matches live order
QueryMT SHALL replay a session's durable events in the same order in which they were delivered live.

#### Scenario: Load after a run with concurrent events
- **WHEN** a client loads a session whose last run produced durable events within the same millisecond
- **THEN** the replayed notifications appear in the same order the live connection received them

### Requirement: Prompt response follows its updates
QueryMT SHALL send a `session/prompt` response on the requesting connection only after that connection has delivered every session update the prompt produced before completing. This SHALL hold for locally hosted and remote-hosted sessions.

#### Scenario: Local prompt
- **WHEN** a client prompts a locally hosted session and the run streams an assistant message
- **THEN** the client receives every update of that message before the prompt response

#### Scenario: Remote prompt
- **WHEN** a client attached to node A prompts a session hosted on node B
- **THEN** the client receives every update relayed from node B for that prompt before the prompt response

#### Scenario: Other connections do not delay the response
- **WHEN** two connections observe the same session and one of them is slow
- **THEN** the prompt response to the requesting connection is not delayed by the other connection

### Requirement: Bounded response wait
QueryMT SHALL NOT withhold a prompt response indefinitely. If the requesting connection's event delivery for the session is lost, through relay disconnect, consumer lag or a bounded safety limit, QueryMT SHALL send the response and SHALL record that ordering could not be guaranteed.

#### Scenario: Relay disconnects before the updates arrive
- **WHEN** the event relay of a remote-hosted session disconnects while a prompt response is waiting for its updates
- **THEN** the response is sent without waiting further and the disconnect is reported to the client

### Requirement: Streaming latency is not tied to persistence
QueryMT SHALL publish ephemeral streaming events without waiting for durable events to be persisted, while still keeping production order.

#### Scenario: Slow storage during streaming
- **WHEN** durable event persistence is slow while a session streams message deltas
- **THEN** the deltas reach attached connections without waiting for pending durable writes

### Requirement: Lag and gap recovery
A connection that falls behind a session's event stream or detects a missing session sequence SHALL recover the missed durable events from the journal and SHALL continue receiving live events afterwards. A lagging connection SHALL NOT stop receiving session updates for the rest of its lifetime.

#### Scenario: WebSocket connection lags
- **WHEN** a WebSocket connection falls behind the live event buffer during a burst
- **THEN** it receives the missed durable updates in order and keeps receiving later updates

#### Scenario: Gap in relayed sequence
- **WHEN** an attached node receives a relayed event whose session sequence skips a number
- **THEN** it fetches the missing events from the host before delivering later ones
