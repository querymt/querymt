# output-limit-recovery Specification

## Purpose

Defines how the agent chooses the output-token cap and how it detects, escalates, continues after and finally reports replies cut off by that cap, so long steps don't silently end a run.

## Requirements

### Requirement: The initial output cap comes from model metadata
When `max_tokens` is not set in configuration, the agent SHALL use `min(32000, model output limit)` as the cap and record the model output limit as the escalation ceiling. With no known limit, Anthropic models SHALL use 32,000 as both values. An explicitly configured `max_tokens` SHALL be used as-is, and SHALL also be the ceiling.

#### Scenario: Large-output model
- **WHEN** a session uses `claude-sonnet-5-5` (output limit 128,000) with no configured `max_tokens`
- **THEN** requests start at 32,000 with an escalation ceiling of 128,000

#### Scenario: Explicit configuration
- **WHEN** the configuration sets `max_tokens = 20000`
- **THEN** requests use 20,000 and no escalation happens

### Requirement: Replies cut off mid-tool-call are length outcomes
A reply that ends with a length finish reason while tool calls are still unfinished SHALL be treated as an incomplete, length-limited reply. It SHALL NOT be classified as a transient stream failure or retried by the stream-retry policy. Partial output SHALL be kept and unfinished calls SHALL NOT run.

#### Scenario: Stream ends inside a tool call
- **WHEN** the provider sends a length stop while a tool call's arguments are incomplete
- **THEN** no retry wait occurs, the call is not executed, and the reply goes to output-limit recovery

### Requirement: Middleware can recover from stops
Before a run stop is published, the agent SHALL give middleware the chance to replace it with a continuation. After at most a configurable number of such continuations per turn (default 5), the stop SHALL be published unchanged.

#### Scenario: Middleware continues
- **WHEN** a stop occurs and a middleware returns a continuation
- **THEN** no stop event is emitted and the run continues with the next model request

#### Scenario: Guard reached
- **WHEN** middleware has already continued the maximum number of times in the turn
- **THEN** the stop is published

### Requirement: A cut-off reply is retried once at the ceiling
When a reply ends with a length finish reason, the cap used was below the escalation ceiling, and no escalation has happened yet in the turn, the agent SHALL discard the reply and retry the same request with the cap set to the ceiling. The discarded reply SHALL NOT appear in history and its usage SHALL be counted.

#### Scenario: Escalation
- **WHEN** a 32,000-token request with ceiling 128,000 is cut off
- **THEN** the same request is sent again with `max_tokens = 128000`, and session usage includes both attempts

### Requirement: Cut-off replies at the ceiling are continued with guidance
When a reply is cut off at the ceiling, the agent SHALL continue the run with a temporary instruction to resume without recap and split large writes into smaller tool calls, up to a configurable number of times per turn (default 3). Thinking-only replies SHALL NOT be stored, and their continuation SHALL use lower reasoning effort when the provider honours it. The instruction SHALL be removed after the next reply that is not cut off.

#### Scenario: Thinking-only cut-off at the ceiling
- **WHEN** a reply at the ceiling contains only thinking and is cut off
- **THEN** the reply is not stored, the next request includes the instruction and uses medium or lower effort, and the run continues

#### Scenario: Instruction removed after success
- **WHEN** the reply after a continuation finishes normally
- **THEN** later requests no longer contain the instruction

#### Scenario: Continuations exhausted
- **WHEN** the reply is cut off again after the configured number of continuations
- **THEN** the run stops with stop type model token limit

### Requirement: Recovery steps are observable
Each escalation or continuation SHALL emit a durable recovery event with the turn, attempt number, action (escalate or continue), the cap before and after, and whether the reply was thinking-only.

#### Scenario: Escalation event
- **WHEN** the agent escalates from 32,000 to 128,000
- **THEN** a recovery event with action escalate, from 32,000 and to 128,000 is recorded in the session journal

### Requirement: Run outcomes distinguish stops from completion
A run that ends on a stop SHALL report outcome `stopped` with the stop type and reason, not `completed`.

#### Scenario: Run ends on output limit
- **WHEN** recovery is exhausted and the run ends
- **THEN** the run-completed event reports outcome `stopped` and stop type model token limit
