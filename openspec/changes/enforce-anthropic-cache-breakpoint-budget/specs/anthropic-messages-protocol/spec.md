# Spec Delta

## ADDED Requirements

### Requirement: Requests carry at most four cache breakpoints
An Anthropic request SHALL contain at most four `cache_control` markers across `system`, `tools` and `messages`. This SHALL hold for portable cache hints, native `cache_control` on system blocks, and both combined. A hint SHALL count only when it produces a marker on the wire.

#### Scenario: Five requested breakpoints are reduced to four
- **WHEN** a request has three hinted system sections and its last two messages are hinted
- **THEN** the sent request contains exactly four `cache_control` markers
- **AND** the request is not rejected for exceeding the breakpoint limit

#### Scenario: Requests within the limit are unchanged
- **WHEN** a request has two hinted system sections and two hinted messages
- **THEN** all four markers are sent where they were requested

#### Scenario: Hints that place no marker do not use the budget
- **WHEN** a hinted message ends in a `redacted_thinking` block or has no content blocks, and four other breakpoints are requested
- **THEN** those four other markers are all sent

### Requirement: Excess breakpoints are dropped by reuse priority
When more than four markers are requested, the provider SHALL keep, in priority order:
1. the last marked system block;
2. the last marked message block, then the second-to-last;
3. the first marked system block;
4. remaining message markers, newest first;
5. remaining system markers, last first.

It SHALL remove all other markers and log one warning stating how many it dropped.

#### Scenario: Middle system markers are dropped first
- **WHEN** a request marks system sections 1 to 4 and its last two messages
- **THEN** markers remain on system sections 1 and 4 and on the last two messages
- **AND** no marker remains on system sections 2 or 3
- **AND** one warning reports two dropped breakpoints

#### Scenario: Older history markers are dropped before recent ones
- **WHEN** a request marks one system section and messages 1, 5, 9 and 10 of ten
- **THEN** markers remain on the system section and on messages 10, 9 and 5
- **AND** the marker on message 1 is removed

#### Scenario: OAuth identity blocks do not shift system priority
- **WHEN** an OAuth request has unmarked identity and billing blocks inserted before two marked configured system sections
- **THEN** the first and last configured system sections are the first and last marked system blocks

### Requirement: Breakpoint lifetimes never increase in request order
Markers in the sent request, ordered as system and then messages, SHALL have non-increasing lifetimes. A marker with a shorter or default lifetime that precedes a longer one SHALL be sent with the longest lifetime that follows it. This SHALL be applied after excess markers are dropped.

#### Scenario: Default system marker before a one-hour marker is raised
- **WHEN** system section 1 has a default hint and system section 2 has a hint with `ttl_seconds = 1800`
- **THEN** both system markers are sent with `ttl` `1h`

#### Scenario: Longer lifetime before shorter is preserved
- **WHEN** system section 1 has a hint with `ttl_seconds = 1800` and the last message has a default hint
- **THEN** the system marker is sent with `ttl` `1h` and the message marker without a `ttl`
