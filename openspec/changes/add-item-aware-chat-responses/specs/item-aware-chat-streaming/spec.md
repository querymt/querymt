## Purpose

Provide item-aware streaming within existing chat streams with reliable completion semantics and duplicate-free canonical output.

## ADDED Requirements

### Requirement: Structured events preserve item and part boundaries
Item-aware streams SHALL identify response metadata, output-item indexes, item identities, relevant content or summary indexes, incremental content, and authoritative completed items. Structured mode SHALL be declared before semantic deltas so consumers can select the correct accumulation path.

#### Scenario: Interleaved parallel calls and reasoning
- **WHEN** events interleave argument deltas for two calls with reasoning and message-part deltas
- **THEN** consumers can associate each delta with the correct item and part
- **AND** completed output retains output-index order rather than event-arrival grouping.

### Requirement: Compatibility events do not duplicate canonical output
The system SHALL support legacy display/tool projections alongside structured events. In structured mode, only structured events SHALL determine canonical history; otherwise legacy events SHALL produce a limited fallback. Tool execution SHALL occur once per validated call and not independently from both representations.

#### Scenario: Both event representations arrive
- **WHEN** a call and text appear as structured events and legacy projection events in one stream
- **THEN** final history contains one call and one text representation
- **AND** the call is dispatched at most once after successful response-level validation.

### Requirement: Completed snapshots are authoritative
Completed item snapshots SHALL replace provisional item state without re-appending displayed deltas. Identical repeated completions SHALL be idempotent; conflicting repeated completions SHALL fail validation. Final usage SHALL NOT be double-counted across snapshots and compatibility events.

#### Scenario: Item and response snapshots repeat arguments
- **WHEN** argument deltas are followed by item completion and a response snapshot containing the same call
- **THEN** the final call contains the authoritative raw arguments once and usage is accounted once.

#### Scenario: Conflicting completion
- **WHEN** the same output index is completed twice with different call identities
- **THEN** the stream reports a protocol validation failure rather than dispatching either as a second call.

### Requirement: Semantic terminal state determines success
The system SHALL distinguish successful response completion from item completion, transport framing, premature EOF, failed responses, and incomplete responses. All final items, status metadata, and usage SHALL precede the public terminal event. Partial output SHALL remain distinguishable from successful executable output.

#### Scenario: Transport closes before semantic completion
- **WHEN** the connection closes after an item completion or a framing marker but before a valid response terminal
- **THEN** the result is a stream failure and pending calls are not dispatched.

#### Scenario: Output token limit
- **WHEN** the provider terminates with an incomplete response caused by its output token limit
- **THEN** consumers receive partial output and the token-limit cause, not successful completion or an unconditional transient-retry classification.

### Requirement: Framing and retries preserve isolation
The streaming path SHALL handle transport splitting across UTF-8, JSON, and SSE frame boundaries. Each request attempt SHALL have isolated accumulation state; retries SHALL NOT mix old partial items with new output.

#### Scenario: Split payload followed by retry
- **WHEN** an event is split across byte chunks and an attempt later fails and is retried
- **THEN** the split event is decoded correctly within its attempt
- **AND** the final retry output contains no items retained from the failed attempt.
