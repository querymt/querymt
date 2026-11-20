# Spec Delta

## Purpose

Defines which trace spans the agent exports at the default telemetry level and which attributes model request spans carry, so that exported traces describe work rather than internal dispatch and retries can be identified.

## ADDED Requirements

### Requirement: Actor message dispatch spans are not exported by default
When `QMT_TELEMETRY_LEVEL` is unset or names only a global level, the agent SHALL NOT export a span for the dispatch of an individual actor message (`actor.handle_message`). Spans created while such a message is handled SHALL still be exported, attached to their nearest exported ancestor or as trace roots. A `QMT_TELEMETRY_LEVEL` value with an explicit `kameo` directive SHALL be applied as written.

#### Scenario: Default level exports no dispatch spans
- **WHEN** telemetry is enabled with `QMT_TELEMETRY_LEVEL` unset and a session runs a turn
- **THEN** no `actor.handle_message` span is exported

#### Scenario: Work inside a handler is still exported
- **WHEN** telemetry is enabled at the default level and an actor message applies workspace index changes
- **THEN** the `workspace_actor.apply_changes` span is exported

#### Scenario: Explicit directive exports dispatch spans
- **WHEN** telemetry is enabled with `QMT_TELEMETRY_LEVEL=info,kameo=info` and a session runs a turn
- **THEN** `actor.handle_message` spans are exported

#### Scenario: Stricter global level still applies
- **WHEN** telemetry is enabled with `QMT_TELEMETRY_LEVEL=error`
- **THEN** no `actor.handle_message` span is exported and no info-level span is exported

### Requirement: Per-step bookkeeping spans are exported only at debug level
The spans `middleware.driver`, `middleware.phase`, `agent.execution.middleware.turn_start`, `agent.execution.middleware.step_start`, `agent.execution.middleware.after_llm`, `agent.execution.middleware.processing_tool_calls`, `agent.execution.middleware.turn_end`, `agent.tool.side_effects`, `agent.request.cache_breakpoints`, `model_inventory.get_snapshot` and `function_index.remove_file` SHALL be exported only when the telemetry level for their target is `debug` or more verbose.

#### Scenario: Default level omits bookkeeping spans
- **WHEN** telemetry is enabled at the default level and a session runs a turn with three model requests and five tool calls
- **THEN** none of the listed span names is exported

#### Scenario: Debug level includes bookkeeping spans
- **WHEN** telemetry is enabled with `QMT_TELEMETRY_LEVEL=debug` and a session runs a turn
- **THEN** `middleware.driver` and `agent.tool.side_effects` spans are exported

#### Scenario: Work spans are unaffected
- **WHEN** telemetry is enabled at the default level and a session runs a turn with a model request and a `shell` call
- **THEN** the model request span, the `execute_tool shell` span and the `agent.tool.invoke` span are exported with the same names and attributes as before

### Requirement: Model request spans report their attempts
Every exported model request span SHALL carry the integer attribute `gen_ai.request.attempt`, holding the number of attempts made for that request, and one span event of the same name per attempt, in order. Retries after provider-reported failures, transport failures and broken streams SHALL count as attempts. The event of a failed attempt SHALL carry the failure message. A reply re-requested with a higher output cap SHALL be a new request with its own span.

#### Scenario: Request without a retry
- **WHEN** a model request succeeds on its first attempt
- **THEN** its span carries `gen_ai.request.attempt = 1` and one `gen_ai.request.attempt` event

#### Scenario: Retry after an overloaded error
- **WHEN** a model request fails with an overloaded error and succeeds when retried
- **THEN** its span carries `gen_ai.request.attempt = 2` and two `gen_ai.request.attempt` events
- **AND** the first event carries the overloaded error message

#### Scenario: Stream recreated after a break
- **WHEN** a streaming response breaks and the request is sent again and succeeds
- **THEN** the span carries `gen_ai.request.attempt = 2`

#### Scenario: Every attempt fails
- **WHEN** a model request fails on all three of its allowed attempts
- **THEN** its span carries `gen_ai.request.attempt = 3`, three events and error status

#### Scenario: Output cap escalation is not a retry
- **WHEN** a reply is cut off, discarded and requested again at the escalation ceiling
- **THEN** the second request has its own span carrying `gen_ai.request.attempt = 1`
