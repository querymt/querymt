# Spec Delta

## Purpose

Defines the delivery guarantees for provider streams relayed between mesh peers, so the client receives stream data in the order the host produced it, exactly once, or gets a clear transport failure.

## ADDED Requirements

### Requirement: Relayed stream data is delivered in host order
The receiving peer SHALL deliver sequenced relay messages for a request to the stream consumer in the order the host produced them, regardless of the order in which the transport delivers them. A message that arrives before an earlier one SHALL be held until every earlier message has been delivered.

#### Scenario: Batches arrive swapped
- **WHEN** the host sends batch 1 then batch 2 and the transport delivers batch 2 first
- **THEN** the consumer receives batch 1 before batch 2
- **AND** the accumulated response equals the one produced without relaying.

#### Scenario: Random delivery order
- **WHEN** all sequenced messages of a complete structured response are delivered in an arbitrary permutation
- **THEN** the consumer receives them in host order and accumulation completes successfully.

### Requirement: Duplicate relay messages are discarded
The receiving peer SHALL deliver each sequenced relay message at most once per request. A message whose sequence number was already delivered or is already held SHALL be discarded without affecting the stream.

#### Scenario: Resent batch after an unclear send failure
- **WHEN** the host resends a batch that the receiving peer already delivered
- **THEN** the consumer does not receive the batch a second time
- **AND** no text or arguments are duplicated in the accumulated output.

### Requirement: Terminals are released only after all earlier data
A terminal relay message (response terminal, provider error or transport failure produced by the host) SHALL be delivered only after every relay message the host sequenced before it. After a terminal is delivered, the stream SHALL close and later messages for that request SHALL be discarded.

#### Scenario: Terminal overtakes the last item completion
- **WHEN** the batch carrying the response terminal arrives before the batch carrying an item completion that the host sent earlier
- **THEN** the consumer receives the item completion before the terminal
- **AND** the response does not fail with unfinished output indexes.

### Requirement: Unfilled gaps end the stream with a transport failure
If a gap in the sequence is not filled within a bounded wait, or the number of held messages exceeds a bounded limit, the receiving peer SHALL end the stream with a retryable transport failure that identifies the missing sequence number. It SHALL NOT deliver held messages past the gap.

#### Scenario: Lost batch
- **WHEN** a sequenced message never arrives and later messages are held beyond the wait
- **THEN** the consumer receives a transport failure naming the missing sequence number
- **AND** the failure is not reported as a stream accumulation error.

#### Scenario: Gap filled in time
- **WHEN** a missing message arrives before the wait elapses
- **THEN** held messages are released in order and no failure is raised.

### Requirement: Slow consumers do not lose or reorder data
Delivery from the receiving peer to the local stream consumer SHALL NOT drop messages or let later messages overtake earlier ones when the consumer reads slowly.

#### Scenario: Consumer stalls during a burst
- **WHEN** the host relays more messages than the consumer reads during a pause
- **THEN** after the pause the consumer receives every message in host order.

### Requirement: Unsequenced and mixed-version relays keep current behaviour
Relay messages without a sequence number, such as heartbeats, connection notices, or messages from a host without sequencing, SHALL be delivered immediately as received. A peer without sequencing SHALL be able to decode messages from a peer with sequencing, and the reverse.

#### Scenario: Old host, new client
- **WHEN** a host that does not sequence messages streams to a client with ordered delivery
- **THEN** messages are delivered as received and the stream completes as before.

#### Scenario: New host, old client
- **WHEN** a host that sequences messages streams to a client without ordered delivery
- **THEN** the client decodes every message and ignores the sequence number.

#### Scenario: Heartbeat during a held gap
- **WHEN** a heartbeat arrives while sequenced messages are held for a gap
- **THEN** the heartbeat is handled immediately and does not release or reorder held data.
