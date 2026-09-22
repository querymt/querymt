## Purpose

Define canonical input and generated-output contracts that preserve provider semantics without retaining parallel legacy content representations.

## ADDED Requirements

### Requirement: Chat generation returns one canonical output value
The system SHALL provide item-aware generation through the existing provider composition without adding a protocol-specific provider capability supertrait. Chat generation SHALL return provider-neutral structured output directly. Text, visible reasoning, executable function calls, usage, and finish reason SHALL be derived projections of that output rather than independently implemented response fields. Providers without native item semantics SHALL normalize their response into a limited structured output without claiming unavailable identities or continuation state.

#### Scenario: Provider without native item semantics
- **WHEN** a provider produces only legacy text, reasoning, function-call, usage, and finish information
- **THEN** its adapter returns a canonical output containing the information it can represent
- **AND** callers use the same output and projection APIs as for an item-aware provider
- **AND** the result does not claim lossless native item or continuation semantics.

### Requirement: Chat input uses one canonical part model
The public chat input API SHALL represent ordinary input as ordered text, validated attachment, and correlated tool-result parts. It SHALL NOT expose separate image, image-URL, PDF, audio, or resource-link variants alongside the attachment model. Generated reasoning and function calls SHALL exist only as structured output items and SHALL NOT also be accepted as ordinary input-content variants. The legacy recursive content enum SHALL NOT remain in the primary public API.

#### Scenario: Caller supplies mixed multimodal input
- **WHEN** a caller creates a prompt containing text, an inline image, a URI-referenced document, and a tool result
- **THEN** text uses the canonical text part, both resources use the same validated attachment type with different metadata or sources, and the result uses the dedicated correlated tool-result type
- **AND** no legacy image, PDF, audio, resource-link, reasoning, or tool-use content variant is required.

#### Scenario: Caller replays generated reasoning and calls
- **WHEN** a previous assistant turn contains reasoning and function-call output items
- **THEN** replay obtains them from that turn's structured output payload
- **AND** the public input-part API cannot create duplicate reasoning or function-call content blocks.

### Requirement: Tool results have a bounded canonical shape
A tool result SHALL carry its call ID, optional function name, error state, and ordered result parts. Result parts SHALL support text and validated attachments without recursively accepting generated reasoning, function calls, or nested tool results. Provider codecs SHALL reject unsupported result attachments explicitly rather than silently dropping or stringifying them.

#### Scenario: Rich tool result
- **WHEN** a tool returns text, an image, and a document for a prior call
- **THEN** the canonical result preserves their order, attachment metadata, call correlation, and error state
- **AND** it cannot contain a nested function call or another tool result.

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

### Requirement: Attachments have typed sources and rendering metadata
Canonical attachments SHALL expose a broad media kind, typed source, optional validated media type, optional filename, description, detail, and scoped provider extensions. The same attachment type SHALL be usable in ordinary input, tool-result parts, and normalized generated message parts. Sources SHALL distinguish inline bytes, data URLs, general URIs, and provider file references; a URI SHALL NOT be assumed to be an HTTP URL. Inline bytes SHALL require a media type. Data URLs SHALL use their declared type or standard default and reject conflicting separately supplied types. URIs and provider file references MAY omit unavailable media types. These invariants SHALL be enforced equally by public construction, mutation, and deserialization so serialized input cannot create a state rejected by normal constructors. Normalization SHALL NOT implicitly fetch resources or sniff bytes. Provider references SHALL remain origin-scoped. Legacy histories SHALL remain readable; invalid legacy MIME strings SHALL fail explicitly when migrated rather than blocking unrelated history loading.

#### Scenario: Renderable attachment metadata
- **WHEN** recognized inline image or document media is normalized
- **THEN** consumers receive typed media with its bytes, kind, validated media type, and supplied filename/detail instead of a placeholder string.

#### Scenario: Referenced attachment without known MIME
- **WHEN** an attachment contains only a general URI or provider file reference with no MIME metadata
- **THEN** its URI, metadata, source form, and origin scope are preserved without inventing a media type or fetching its bytes
- **AND** consumers can present a resource or attachment without a guarantee of inline rendering.

#### Scenario: Conflicting data URL metadata
- **WHEN** an attachment declares image/jpeg separately but its data URL declares image/png, whether supplied through a constructor or serialized input
- **THEN** construction or deserialization reports a conflict instead of creating an invalid attachment or silently choosing one declaration.

#### Scenario: Deserialized inline media omits its type
- **WHEN** serialized structured media contains inline bytes without a concrete media type
- **THEN** deserialization fails with the same invariant violation as direct construction.

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

### Requirement: Assistant history has one authoritative payload
An assistant turn SHALL contain exactly one authoritative payload: canonical input parts used as a portable projection or structured output. Portable input parts for display, legacy serialization, or cross-origin replay SHALL be derived on demand from structured output and SHALL NOT be stored as an independently mutable second source of truth. The public API SHALL make replacement or conversion between payload forms explicit rather than relying on request-time consistency checks between public fields.

#### Scenario: Structured turn is replayed once
- **WHEN** an assistant turn containing structured reasoning, text, and calls is converted into a provider request
- **THEN** each item is serialized once in order
- **AND** a display or portable projection is not appended as additional history.

#### Scenario: Caller intentionally makes a portable edit
- **WHEN** a caller converts a structured assistant turn to canonical portable input parts and edits its text
- **THEN** the turn no longer carries the discarded structured continuation state
- **AND** replay cannot silently use stale output hidden behind the edited content.

### Requirement: Output fidelity is derived from retained semantics
The system SHALL determine native replay and transport requirements from the concrete identities, provenance, opaque continuation, and other retained semantics in an output. Correctness SHALL NOT depend on a caller-set marker that labels an otherwise arbitrary output as structured or legacy.

#### Scenario: Basic normalized output crosses a legacy boundary
- **WHEN** an output synthesized from portable text and calls contains no item-aware-only continuation semantics
- **THEN** transport capability checks do not reject it merely because it uses the canonical output type.

#### Scenario: Output requires item-aware fidelity
- **WHEN** an output contains identities, opaque state, ordering, or continuation semantics that a legacy boundary cannot preserve
- **THEN** the boundary requires the item-aware contract or returns an explicit compatibility error.

### Requirement: Portable projections do not execute opaque items
Portable accessors SHALL expose display text, visible reasoning, and supported local function calls without interpreting opaque provider items as executable functions. Projections SHALL NOT claim lossless preservation of provider-only semantics.

#### Scenario: Unknown provider action
- **WHEN** an output item is not a supported local function call
- **THEN** it is retained as structured data but is absent from the executable local function-call projection.
