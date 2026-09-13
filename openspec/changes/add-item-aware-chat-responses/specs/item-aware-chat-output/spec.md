## Purpose

Preserve generated conversation structure within existing chat interfaces while retaining usable legacy text and function-call projections.

## ADDED Requirements

### Requirement: Existing provider composition remains sufficient
The system SHALL provide item-aware generation through existing chat generation interfaces without adding a required provider capability supertrait or replacing existing generation method signatures. Legacy response implementations SHALL default to structured output being unavailable.

#### Scenario: Legacy provider remains usable
- **WHEN** a provider implements the existing required generation and response methods without item-aware support
- **THEN** callers can use it through the existing aggregate provider interface
- **AND** normalization identifies its output as a limited legacy projection rather than claiming lossless item data.

### Requirement: Ordered output preserves protocol semantics
Structured output SHALL preserve ordered message, reasoning, function-call, and opaque items; distinct item and call identifiers; item status; message phase; ordered content parts; annotations; refusals; and unknown semantic JSON fields. Encrypted reasoning and signatures SHALL remain separate from visible summaries and SHALL survive when summaries are empty.

#### Scenario: Mixed generated output
- **WHEN** output contains reasoning, a message with annotated text, two function calls, another reasoning item with only encrypted content, and an unknown item
- **THEN** normalization retains each item's identity, fields, and relative order
- **AND** no empty-summary reasoning item is discarded or merged into another reasoning item.

### Requirement: Raw function arguments remain authoritative
The system SHALL preserve exact raw argument strings independent of whether they parse as JSON. Parsing failures SHALL NOT replace arguments with an empty object or authorize execution of an invalid local function call.

#### Scenario: Invalid arguments survive inspection
- **WHEN** a completed function item contains invalid JSON arguments
- **THEN** structured output retains the original argument string
- **AND** a request to parse or execute it returns an explicit validation failure.

### Requirement: Structured history has one source of truth
When a generated turn has structured output, the system SHALL treat it as authoritative and derive portable content from it. The system SHALL NOT serialize both structured output and its compatibility projection as additional history. A disagreement caused by editing only the projection SHALL be rejected unless the edit explicitly clears or replaces structured output.

#### Scenario: Structured turn is replayed once
- **WHEN** an assistant turn containing structured reasoning, text, and calls is converted into a provider request
- **THEN** each item is serialized once in order
- **AND** projected text and calls are not appended again.

#### Scenario: Caller edits projected text
- **WHEN** a caller changes a structured turn's projected content without clearing or updating its authoritative output
- **THEN** request validation reports inconsistent history rather than silently replaying stale content.

### Requirement: Portable projections do not execute opaque items
Portable accessors SHALL expose display text, visible reasoning, and supported local function calls without interpreting opaque provider items as executable functions. Projections SHALL NOT claim lossless preservation of provider-only semantics.

#### Scenario: Unknown provider action
- **WHEN** an output item is not a supported local function call
- **THEN** it is retained as structured data but is absent from the executable local function-call projection.
