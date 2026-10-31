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
A reply SHALL be treated as cut off by the output-token limit when its finish reason is `length`, regardless of the output status the provider reports and whether the provider flushed interrupted tool calls as completed. A cut-off reply SHALL NOT be classified as a transient stream failure or retried by the stream-retry policy. Partial output SHALL be kept. No tool call from a cut-off reply SHALL run, including calls that completed before the limit. Each call SHALL receive exactly one error result before the run stops with stop type model token limit: a call whose input is invalid or was interrupted SHALL receive the `INVALID_JSON` result defined for invalid tool input, extended with guidance to split large writes into smaller calls; any other call SHALL receive a skipped result carrying the same guidance.

#### Scenario: Stream ends inside a tool call
- **WHEN** the provider sends a length stop while a tool call's arguments are incomplete
- **THEN** no retry wait occurs, the call is not executed, and the reply goes to output-limit recovery

#### Scenario: Provider flushes the interrupted block as completed
- **WHEN** a streamed reply ends with a length stop and the provider reports the interrupted tool call as completed but invalid
- **THEN** the call is not executed, it receives one error result containing `INVALID_JSON` and the split guidance, and the next request answers that call before any other message

#### Scenario: Call completed before the limit
- **WHEN** a valid tool call completes and a later part of the same reply is cut off by the length limit
- **THEN** the valid call is not executed and receives one skipped result with the split guidance

### Requirement: Middleware can recover from stops
Before a run stop is published, the agent SHALL give middleware the chance to replace it with a continuation. After at most a configurable number of such continuations per turn (default 5), the stop SHALL be published unchanged.

#### Scenario: Middleware continues
- **WHEN** a stop occurs and a middleware returns a continuation
- **THEN** no stop event is emitted and the run continues with the next model request

#### Scenario: Guard reached
- **WHEN** middleware has already continued the maximum number of times in the turn
- **THEN** the stop is published

### Requirement: A cut-off reply is retried once at the ceiling
When a reply ends with a length finish reason, the cap used was below the escalation ceiling, and no escalation has happened yet in the turn, the agent SHALL discard the reply and retry the same request with the cap set to the ceiling, limited to the output room left in the context window. The discarded reply SHALL NOT appear in history and its usage SHALL be counted. After an escalation, every later request in the same turn SHALL use the escalated cap unless another override sets a cap, re-limited to the context room at the time of each request. The discard-and-retry SHALL happen at most once per turn. The next turn SHALL start at the initial cap.

#### Scenario: Escalation
- **WHEN** a 32,000-token request with ceiling 128,000 is cut off
- **THEN** the same request is sent again with `max_tokens = 128000`, and session usage includes both attempts

#### Scenario: Escalated cap lasts for the turn
- **WHEN** an escalated request succeeds with tool calls and the tool results are sent in the same turn
- **THEN** that request uses `max_tokens = 128000`, and a cut-off of it goes to continuation instead of a second escalation

#### Scenario: Next turn starts low
- **WHEN** a turn escalated to 128,000 ends and the user sends a new prompt
- **THEN** the first request of the new turn uses 32,000

### Requirement: Cut-off replies that cannot be escalated are continued with guidance
When a reply is cut off and cannot be escalated, because escalation was already spent in the turn, the cap used is at the ceiling, the provider does not honour a per-request cap, or no context room is left above the cap, the agent SHALL continue the run with a temporary instruction to resume without recap and split large writes into smaller tool calls, up to a configurable number of times per turn (default 3). Thinking-only replies SHALL NOT be stored, and their continuation SHALL use lower reasoning effort when the provider honours it. The instruction SHALL be removed after the next reply that is not cut off.

#### Scenario: Thinking-only cut-off at the ceiling
- **WHEN** a reply at the ceiling contains only thinking and is cut off
- **THEN** the reply is not stored, the next request includes the instruction and uses medium or lower effort, and the run continues

#### Scenario: Cut-off after escalation was spent
- **WHEN** a turn has already escalated and a later reply in the turn is cut off
- **THEN** the run continues with the instruction at the escalated cap

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
