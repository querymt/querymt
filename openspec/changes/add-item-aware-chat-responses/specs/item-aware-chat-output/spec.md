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

### Requirement: Media types use validated crate-backed values
The structured media contract SHALL use a QueryMT-owned MediaType backed by the mime crate, with fallible parsing and parsed MIME accessors. It SHALL serialize as a string in persistence, plugin/remote DTOs, and bindings, with a string JSON schema. Concrete attachment types SHALL reject invalid syntax and wildcard ranges. Parameter semantics SHALL survive normalization and roundtrip; canonical spelling MAY differ. MIME syntax validation SHALL NOT imply endpoint support or verification of the actual bytes.

#### Scenario: Parameterized media type roundtrip
- **WHEN** a typed attachment with media type text/plain; charset=utf-8 passes through storage and supported transports
- **THEN** its parsed type, subtype, and charset parameter retain their meaning
- **AND** the wire value remains a MIME string rather than the dependency's internal representation.

#### Scenario: Invalid or wildcard media type
- **WHEN** a caller constructs or deserializes a structured attachment with malformed MIME syntax or image/*
- **THEN** validation fails explicitly instead of storing it as a concrete MediaType.

### Requirement: Recognized media has typed sources and rendering metadata
Recognized media SHALL expose a broad media kind, typed source, optional validated media type, optional filename/detail, and scoped provider extensions. Inline bytes SHALL require a media type. Data URLs SHALL use their declared type or standard default and reject conflicting separately supplied types. URLs and provider file references MAY omit unavailable media types. Normalization SHALL NOT implicitly fetch resources or sniff bytes. Provider references SHALL remain origin-scoped. Legacy histories SHALL remain readable; invalid legacy MIME strings SHALL fail explicitly when converted to structured media rather than blocking legacy loading.

#### Scenario: Renderable attachment metadata
- **WHEN** recognized inline image or document media is normalized
- **THEN** consumers receive typed media with its bytes, kind, validated media type, and supplied filename/detail instead of a placeholder string.

#### Scenario: Referenced media without known MIME
- **WHEN** a recognized media item contains only a URL or provider file reference with no MIME metadata
- **THEN** its source form and origin scope are preserved without inventing a media type or fetching its bytes
- **AND** consumers can present a reference or attachment without a guarantee of inline rendering.

#### Scenario: Conflicting data URL metadata
- **WHEN** an attachment declares image/jpeg separately but its data URL declares image/png
- **THEN** validation reports a conflict instead of silently choosing one declaration.

### Requirement: Media formats can grow without losing unknown items
Additional formats within an existing media kind SHALL be representable by MIME value without a new core variant per format. Typed valid media without a specialized renderer SHALL permit a generic attachment presentation. Unknown item/part semantics SHALL remain opaque, not be guessed from embedded MIME fields. Later explicit codec support MAY derive a typed projection from stored opaque data but SHALL retain canonical identity, order, and original replay data without duplicate items or implicit execution.

#### Scenario: Valid format without endpoint support
- **WHEN** typed media uses a valid format unsupported by the selected endpoint
- **THEN** it remains preservable as an attachment
- **AND** request construction reports unsupported content rather than substituting text or silently dropping it.

#### Scenario: Later recognition of stored opaque media
- **WHEN** an explicitly added codec recognizes a previously stored opaque media item
- **THEN** it can produce a typed display projection while retaining the original canonical item and replay metadata
- **AND** it neither creates a second history item nor dispatches a local tool.

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
