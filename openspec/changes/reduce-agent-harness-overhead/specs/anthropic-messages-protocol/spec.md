# Spec Delta

## ADDED Requirements

### Requirement: In-conversation tool changes are declared per model
The Anthropic provider SHALL declare `tool_updates = true` in its chat capabilities only when the configured model accepts tool changes inside the conversation, and `false` for every other model. The model is identified by its id without a trailing date suffix. (Confirmed against the Claude API docs, "Mid-conversation system messages and tool changes", and the live endpoint; see tasks 10.1.)

#### Scenario: Model ids that accept tool changes
- **WHEN** the capabilities are read for `claude-opus-4-8`, `claude-opus-5`, `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`, or an id starting with `claude-fable-5` or `claude-mythos-5`
- **THEN** `tool_updates` is true

#### Scenario: Supporting model declares tool updates
- **WHEN** the capabilities of an Anthropic provider are read for a model that accepts in-conversation tool changes
- **THEN** `tool_updates` is true

#### Scenario: Other model declares no tool updates
- **WHEN** the capabilities of an Anthropic provider are read for a model that does not accept in-conversation tool changes
- **THEN** `tool_updates` is false

#### Scenario: Sibling model id does not inherit support
- **WHEN** the capabilities are read for `claude-sonnet-5` and for `claude-haiku-4-5-20251001`
- **THEN** `tool_updates` is false for both

#### Scenario: Dated id of a supporting model
- **WHEN** the capabilities are read for a supporting model id with a trailing date suffix
- **THEN** `tool_updates` is true

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

### Requirement: Strict limits count the tools in effect
The request-wide strict limits SHALL be applied to the strict tools in effect after each tool-change entry: tools the entry removes or redefines no longer count, strict tools it adds do. A strict tool that would exceed a limit SHALL be sent without the strict flag. An entry's flags SHALL depend only on the entries before it.

#### Scenario: Removal frees a strict slot
- **WHEN** the top-level `tools` hold 20 strict tools and a tool-change message removes one and adds a strict tool
- **THEN** the added tool is sent with the strict flag

#### Scenario: Redefinition counts once
- **WHEN** the top-level `tools` hold 20 strict tools and a tool-change message redefines one of them as a strict tool
- **THEN** the redefined tool is sent with the strict flag

#### Scenario: Strict tool over the limit
- **WHEN** the top-level `tools` hold 20 strict tools and a tool-change message adds another strict tool without removing one
- **THEN** the added tool is sent without the strict flag

### Requirement: Tool-change entries are placed where the API accepts them
The API accepts a `system` entry with tool blocks only directly after a `user` entry (one holding tool results counts), as the last entry or directly before an `assistant` entry. When `user` entries follow a tool-change message in history, the provider SHALL send its entry after that run of `user` entries, keeping the order of tool-change entries. A tool-change message that would be the first entry SHALL fail the request before anything is sent, with an error naming the problem.

#### Scenario: Trailing request-scoped message after a tool change
- **WHEN** a request ends with a tool-change message followed by an unhinted trailing user message
- **THEN** the sent `messages` end with the trailing user entry followed by the `system` entry holding the tool blocks

#### Scenario: New prompt after an interrupted run
- **WHEN** a stored tool-change message is followed by a new user prompt before any assistant reply
- **THEN** the sent `system` entry comes after that prompt and before the next assistant entry

#### Scenario: Tool change directly after tool results
- **WHEN** a tool-change message follows a user message holding tool results
- **THEN** it is sent unchanged in that position

#### Scenario: Tool change first in the conversation
- **WHEN** a request would start with a tool-change message
- **THEN** no HTTP request is made and the error says a tool change cannot be the first message

### Requirement: Tool changes at the same position share one entry
Tool-change messages that are sent at the same position SHALL be sent as one `system` entry holding their blocks in the stored order. A second `system` entry there would not directly follow a `user` entry.

#### Scenario: Several tool changes wait for the same position
- **WHEN** two tool-change messages are stored before the next assistant reply, each after a user message
- **THEN** one `system` entry is sent after the last of those `user` entries
- **AND** it holds the blocks of both changes in the order the changes were stored

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

#### Scenario: Header is added on the OAuth path
- **WHEN** an OAuth-authenticated request contains a tool-change message
- **THEN** `anthropic-beta` contains the OAuth values and `inline-tools-2026-09-15`

### Requirement: Tool-change messages for an unsupported model are rejected before sending
A request that contains a tool-change message for a model that does not accept in-conversation tool changes SHALL fail before anything is sent, with an error naming the model.

#### Scenario: Tool change for an unsupported model
- **WHEN** a request for a model that does not accept in-conversation tool changes contains a tool-change message
- **THEN** no HTTP request is made and the error names the model
