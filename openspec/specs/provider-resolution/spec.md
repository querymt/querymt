# provider-resolution Specification

## Purpose

Defines how a run obtains the provider it talks to and that provider's capabilities, so that every step of a run works with the same provider and the same capabilities and a provider plugin is not asked the same question repeatedly. A run's model is fixed for the run; the tools it offers are not (MCP servers can change them between requests).

## Requirements

### Requirement: A run resolves its model once, before its first middleware hook
A run SHALL resolve its provider, and with it the model's capabilities, output caps and configured reasoning effort, once, before any middleware hook of that run is called, and SHALL use them for every model request of the run. A failure to build the provider SHALL be retried by the rules for provider setup.

#### Scenario: Provider is built after a transient failure
- **WHEN** the first attempt to build the provider fails with a retryable error and the second succeeds
- **THEN** the run continues and sends its model request

#### Scenario: Turn-start hook sees the model's capabilities
- **WHEN** a run starts on a provider whose first build fails and that declares per-request `max_tokens`
- **THEN** the first middleware hook of the run is given a model profile that declares `max_tokens`

#### Scenario: Run cancelled while its provider is resolved
- **WHEN** a run is cancelled while it waits to retry building its provider
- **THEN** the run ends as cancelled and no model request is sent

#### Scenario: Provider is built once for a run with several requests
- **WHEN** a run sends three model requests
- **THEN** the provider is built once for the run

### Requirement: A run whose provider cannot be built fails before any hook
When the provider cannot be built, the run SHALL fail with the provider-initialization error. The user's prompt SHALL already be stored in the session's history, no middleware hook SHALL be called and no model request SHALL be sent. Native `/compact` SHALL fail with the same provider failure before its pre-compaction hook runs and before any compaction event.

#### Scenario: Provider cannot be built
- **WHEN** a run starts and the session's provider cannot be built, on every attempt
- **THEN** the run fails with an error that names provider initialization
- **AND** the user's prompt is in the session's history
- **AND** no middleware hook is called and no model request is sent

#### Scenario: Manual compaction with a provider that cannot be built
- **WHEN** a user runs native `/compact` and the session's provider cannot be built
- **THEN** the command fails with the provider failure
- **AND** no pre-compaction hook runs and no compaction event is emitted

### Requirement: A run's tools are not part of its resolved model
The tools a run offers SHALL be collected for every model request, so that a change to an MCP server's tools takes effect on the next request of a run in progress.

#### Scenario: MCP tools change between two requests of one run
- **WHEN** an MCP server's tool list changes after a run's first model request
- **THEN** the run's second model request offers the changed tool list

### Requirement: A plugin's capabilities are read once per built provider
The agent SHALL ask a provider plugin for its capabilities at most once for each provider built from it, and SHALL use that answer for the provider's lifetime. An attempt that fails SHALL NOT be kept as the answer: the provider SHALL be treated as declaring no capabilities for that read, and the next read SHALL ask the plugin again.

#### Scenario: Several requests through one plugin provider
- **WHEN** a run sends three model requests through a provider built from a plugin
- **THEN** the plugin is asked for its capabilities once

#### Scenario: Failed capability read is not kept
- **WHEN** the plugin fails to answer the first capability read and answers the second
- **THEN** the first read reports no capabilities and the second reports what the plugin declares

#### Scenario: Plugin without a capability declaration
- **WHEN** a provider is built from a plugin that exports no capability declaration
- **THEN** it is treated as before this change: streaming is declared when the plugin exports its stream parser functions, and nothing else is
