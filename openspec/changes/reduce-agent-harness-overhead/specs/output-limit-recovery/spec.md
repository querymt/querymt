# Spec Delta

## MODIFIED Requirements

### Requirement: The initial output cap comes from model metadata
When `max_tokens` is not set in configuration, the agent SHALL use `min(32000, model output limit)` as the cap and record the model output limit as the escalation ceiling. When the session's reasoning effort is `high` or `max` at the time a turn starts, the cap for that turn SHALL instead be `min(64000, model output limit)`. With no known limit, Anthropic models SHALL use 32,000 as both values at every reasoning effort. An explicitly configured `max_tokens` SHALL be used as-is, and SHALL also be the ceiling.

#### Scenario: Large-output model
- **WHEN** a session uses `claude-sonnet-5-5` (output limit 128,000) with no configured `max_tokens` and reasoning effort `medium`
- **THEN** requests start at 32,000 with an escalation ceiling of 128,000

#### Scenario: Explicit configuration
- **WHEN** the configuration sets `max_tokens = 20000`
- **THEN** requests use 20,000 and no escalation happens

#### Scenario: High reasoning effort starts higher
- **WHEN** a session uses a model with output limit 128,000, no configured `max_tokens`, and reasoning effort `high` or `max`
- **THEN** requests start at 64,000 with an escalation ceiling of 128,000

#### Scenario: High effort on a small-output model
- **WHEN** a session uses a model with output limit 40,000, no configured `max_tokens`, and reasoning effort `max`
- **THEN** requests start at 40,000 and no escalation happens

#### Scenario: Effort without a reasoning level
- **WHEN** a session's reasoning effort is `auto`, `low` or unset
- **THEN** requests start at `min(32000, model output limit)`

#### Scenario: Effort changes between turns
- **WHEN** a turn runs at reasoning effort `medium` and the user sets effort `high` before the next prompt
- **THEN** the first turn's requests start at 32,000 and the next turn's requests start at 64,000

#### Scenario: Explicit cap wins at high effort
- **WHEN** the configuration sets `max_tokens = 20000` and reasoning effort is `max`
- **THEN** requests use 20,000 and no escalation happens

## ADDED Requirements

### Requirement: Cut-off replies that cannot be escalated are continued
When a reply is cut off and cannot be escalated, because escalation was already spent in the turn, the cap used is at the ceiling, the provider does not honour a per-request cap, or no context room is left above the cap, the agent SHALL continue the run with an instruction to resume without recap and split large writes into smaller tool calls, up to a configurable number of times per turn (default 3).

#### Scenario: Cut-off after escalation was spent
- **WHEN** a turn has already escalated and a later reply in the turn is cut off
- **THEN** the run continues with the instruction at the escalated cap

#### Scenario: Continuations exhausted
- **WHEN** the reply is cut off again after the configured number of continuations
- **THEN** the run stops with stop type model token limit

### Requirement: Continuation guidance is a stored notice
The continuation instruction SHALL be stored in session history as a notice with source `output_limit`, once per continuation, after any retained partial reply. It SHALL stay in history after later replies and SHALL NOT be removed or rewritten.

#### Scenario: Guidance after a retained partial reply
- **WHEN** a cut-off reply with partial text is retained and the run continues
- **THEN** the instruction is stored after that reply and the reply is sent unchanged

#### Scenario: Instruction stays in history
- **WHEN** the reply after a continuation finishes normally
- **THEN** later requests still contain the instruction at the position where it was stored, and no second copy

### Requirement: Thinking-only cut-off replies are not stored
A cut-off reply that contains only thinking SHALL NOT be stored, and its continuation SHALL use lower reasoning effort when the provider honours it.

#### Scenario: Thinking-only cut-off at the ceiling
- **WHEN** a reply at the ceiling contains only thinking and is cut off
- **THEN** the reply is not stored, the next request includes the instruction and uses medium or lower effort, and the run continues

## REMOVED Requirements

### Requirement: Cut-off replies that cannot be escalated are continued with guidance
**Reason**: It required the continuation instruction to be removed after the next complete reply, which rewrites history between requests and invalidates the prompt cache. The instruction is now stored once and kept.
**Migration**: Replaced by "Cut-off replies that cannot be escalated are continued", "Continuation guidance is a stored notice" and "Thinking-only cut-off replies are not stored", which keep every other rule of this requirement. Nothing to configure.
