## Purpose

Provide one canonical item-aware event stream with reliable cross-transport completion, explicit projection adapters, and duplicate-free output.

## ADDED Requirements

### Requirement: Structured events preserve item and part boundaries
Item-aware streams SHALL identify response metadata, output-item indexes, item identities, relevant content or summary indexes, incremental content, and authoritative completed items. Structured mode SHALL be declared before semantic deltas so consumers can select the correct accumulation path.

#### Scenario: Interleaved parallel calls and reasoning
- **WHEN** events interleave argument deltas for two calls with reasoning and message-part deltas
- **THEN** consumers can associate each delta with the correct item and part
- **AND** completed output retains output-index order rather than event-arrival grouping.

### Requirement: Canonical streams have one event representation
Item-aware providers SHALL emit one canonical structured event representation. Legacy display, UI, and tool-oriented chunks SHALL be produced by an explicit projection adapter for consumers that request them, not interleaved with canonical events. Canonical accumulation SHALL consume only canonical events, and tool execution SHALL occur once per validated call.

#### Scenario: Legacy UI consumes an item-aware stream
- **WHEN** a legacy display consumer is attached through the compatibility adapter
- **THEN** the adapter projects text, visible reasoning, and completed local calls from canonical events
- **AND** the canonical history contains one representation of each item
- **AND** the call is dispatched at most once after successful response-level validation.

### Requirement: Completed snapshots are authoritative
Completed item snapshots SHALL replace provisional item state without re-appending projected display deltas. Identical repeated completions SHALL be idempotent; conflicting repeated completions SHALL fail validation. Final usage SHALL NOT be double-counted across deltas, snapshots, and adapter projections.

#### Scenario: Item and response snapshots repeat arguments
- **WHEN** argument deltas are followed by item completion and a response snapshot containing the same call
- **THEN** the final call contains the authoritative raw arguments once and usage is accounted once.

#### Scenario: Conflicting completion
- **WHEN** the same output index is completed twice with different call identities
- **THEN** the stream reports a protocol validation failure rather than dispatching either as a second call.

### Requirement: Semantic terminal state determines success
The system SHALL distinguish successful response completion from item completion, transport framing, premature EOF, failed responses, and incomplete responses. All final items, status metadata, and usage SHALL precede the public terminal event. Partial output SHALL remain distinguishable from successful executable output. Every supported local or remote stream transport SHALL recognize the canonical response terminal as terminal for acknowledgement, buffering, lifecycle completion, and receiver shutdown.

#### Scenario: Transport closes before semantic completion
- **WHEN** the connection closes after an item completion or a framing marker but before a valid response terminal
- **THEN** the result is a stream failure and pending calls are not dispatched.

#### Scenario: Output token limit
- **WHEN** the provider terminates with an incomplete response caused by its output token limit
- **THEN** consumers receive partial output and the token-limit cause, not successful completion or an unconditional transient-retry classification.

#### Scenario: Structured terminal crosses a remote relay
- **WHEN** a canonical response terminal is sent through a remote stream transport without a legacy done chunk
- **THEN** the relay acknowledges and records the terminal phase
- **AND** the receiving stream closes without waiting for timeout or legacy framing.

### Requirement: Accumulation produces an explicit outcome
Canonical accumulation SHALL consume events into a single attempt-local state and finalize into an explicit completed, incomplete, or failed outcome carrying the available canonical output and terminal detail. Finalization SHALL consume the accumulator so callers cannot continue appending after interpreting a terminal outcome.

#### Scenario: Failed response contains partial items
- **WHEN** a provider reports failure after producing structured items
- **THEN** the failed outcome carries the partial canonical output and classified provider failure
- **AND** unfinished calls are not executable.

### Requirement: Framing and retries preserve isolation
The streaming path SHALL handle transport splitting across UTF-8, JSON, and SSE frame boundaries. Each request attempt SHALL have isolated accumulation state; retries SHALL NOT mix old partial items with new output.

#### Scenario: Split payload followed by retry
- **WHEN** an event is split across byte chunks and an attempt later fails and is retried
- **THEN** the split event is decoded correctly within its attempt
- **AND** the final retry output contains no items retained from the failed attempt.
