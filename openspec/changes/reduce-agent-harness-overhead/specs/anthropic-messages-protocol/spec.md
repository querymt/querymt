# Spec Delta

## ADDED Requirements

### Requirement: In-conversation tool changes are declared per model
The Anthropic provider SHALL declare `tool_updates = true` in its chat capabilities only when the configured model accepts tool changes inside the conversation, and `false` for every other model.

#### Scenario: Supporting model declares tool updates
- **WHEN** the capabilities of an Anthropic provider are read for a model that accepts in-conversation tool changes
- **THEN** `tool_updates` is true

#### Scenario: Other model declares no tool updates
- **WHEN** the capabilities of an Anthropic provider are read for a model that does not accept in-conversation tool changes
- **THEN** `tool_updates` is false

### Requirement: Tool-change messages are sent as system entries with tool blocks
A tool-change message SHALL be sent as one entry in `messages` with role `system`, holding one `tool_addition` block per added or redefined tool followed by one `tool_removal` block per removed tool, each in the order given. The request's top-level `tools` SHALL NOT be changed by a tool-change message.

#### Scenario: Added tool on the wire
- **WHEN** a request contains a tool-change message that adds `deploy`
- **THEN** the sent `messages` contain a `system` entry with one `tool_addition` block holding the definition of `deploy`
- **AND** `deploy` is not in the top-level `tools`

#### Scenario: Removed tool on the wire
- **WHEN** a request contains a tool-change message that removes `deploy`
- **THEN** the sent `messages` contain a `system` entry with one `tool_removal` block naming `deploy`
- **AND** `deploy` is still in the top-level `tools`

#### Scenario: Redefined tool on the wire
- **WHEN** a request contains a tool-change message that carries a new definition of `skill`
- **THEN** the entry has one `tool_addition` block for `skill` and no `tool_removal` block

### Requirement: Tool blocks have a fixed shape
A `tool_addition` block SHALL be `{"type": "tool_addition", "tool": {"type": "tool_definition", "definition": <tool>}}`, where `<tool>` is the tool as it would appear in the top-level `tools`, including its strict flag. A `tool_removal` block SHALL be `{"type": "tool_removal", "tool": {"type": "tool_reference", "name": <name>}}`.

#### Scenario: Strict tool added by a tool change
- **WHEN** a tool-change message adds a tool that would be sent with the strict flag in the top-level `tools`
- **THEN** the `definition` in its `tool_addition` block carries the strict flag and the same schema

#### Scenario: Removal names the tool only
- **WHEN** a tool-change message removes `deploy`
- **THEN** its `tool_removal` block holds a `tool_reference` with `name` `deploy` and no definition

### Requirement: Tool-change entries carry no cache marker
A tool-change entry SHALL carry no `cache_control`, and a cache hint on a tool-change message SHALL NOT count toward the four-breakpoint limit.

#### Scenario: Hinted tool-change message places no marker
- **WHEN** a tool-change message carries a cache hint
- **THEN** its entry is sent without `cache_control` and the hint does not count toward the four-breakpoint limit

### Requirement: Requests with tool-change messages carry the beta header
A request that contains at least one tool-change message SHALL include `inline-tools-2026-09-15` in its `anthropic-beta` header, in addition to any other beta values. A request without a tool-change message SHALL NOT include it.

#### Scenario: Header is added with a tool change
- **WHEN** a request contains a tool-change message and already sends one other beta value
- **THEN** `anthropic-beta` contains both values

#### Scenario: No header without a tool change
- **WHEN** a request contains no tool-change message
- **THEN** `anthropic-beta` does not contain `inline-tools-2026-09-15`

### Requirement: Tool-change messages for an unsupported model are rejected before sending
A request that contains a tool-change message for a model that does not accept in-conversation tool changes SHALL fail before anything is sent, with an error naming the model.

#### Scenario: Tool change for an unsupported model
- **WHEN** a request for a model that does not accept in-conversation tool changes contains a tool-change message
- **THEN** no HTTP request is made and the error names the model
