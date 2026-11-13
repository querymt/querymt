# Spec Delta

## MODIFIED Requirements

### Requirement: Compatibility failures are explicit
Old serialized histories without structured output SHALL remain readable and SHALL normalize at the load boundary. Peers SHALL advertise support before an operation sends semantics that require the item-aware contract; use of the canonical output type alone SHALL NOT require item-aware negotiation when all retained semantics have a lossless legacy representation. An incompatible peer SHALL reject fidelity-requiring operation instead of silently dropping data. Native plugin compatibility SHALL be checked by the exact native plugin ABI version handshake before any provider operation, and structured output SHALL cross the native plugin boundary in the same canonical serialized form used by other transports.

#### Scenario: Older remote or plugin peer
- **WHEN** an item-aware operation targets a peer without support for its contract version
- **THEN** the operation fails with an explicit compatibility error before generation
- **AND** legacy non-item-aware operations remain available.

#### Scenario: Existing stored history
- **WHEN** a history record contains legacy text, image, image-URL, PDF, audio, resource-link, thinking, tool-use, or tool-result content variants
- **THEN** a private migration decoder normalizes user text, resources, and results into canonical input parts
- **AND** it normalizes an entire assistant turn into one structured output payload, including message items for assistant text or attachments and reasoning or function-call items for generated semantics
- **AND** the loaded in-memory model contains no public legacy content value.

#### Scenario: Canonical portable history targets an older peer
- **WHEN** canonical output contains only semantics that the legacy wire contract preserves losslessly
- **THEN** the operation may use an explicit legacy transport projection without falsely requiring item-aware support.

#### Scenario: Item-aware state targets an older peer
- **WHEN** canonical output contains ordering, identity, opaque, or continuation semantics that the legacy wire contract cannot preserve
- **THEN** the operation fails before generation with an explicit compatibility error.

#### Scenario: Structured output through a native plugin
- **WHEN** a native plugin built for the host's ABI version returns item-aware structured output with reasoning, function-call items, opaque fields and provenance
- **THEN** the host receives output equal to what the same provider returns when linked into the host, with item order, IDs and raw arguments preserved.
