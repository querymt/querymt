# agent-middleware Specification

## Purpose

Defines how middleware takes part in an agent run: what state it may keep, what it may decide at each stage of a run, how several middleware are combined, and how the built-in context-usage warning behaves.

## Requirements

### Requirement: Middleware state is scoped to one run
State that a middleware keeps while a run executes SHALL belong to that run only. It SHALL NOT be visible to a run of another session, and SHALL NOT carry over into a later run of the same session. Anything a middleware needs across runs SHALL be read from the session's stored history or its configuration.

#### Scenario: Concurrent sessions do not share middleware state
- **WHEN** two sessions of one agent run at the same time and a middleware records a per-run fact in one of them
- **THEN** the same middleware in the other session behaves as if that fact was never recorded

#### Scenario: Middleware state does not carry into the next run
- **WHEN** a middleware limits itself to one action per run, performs it in a run, and the session then starts another run
- **THEN** the middleware may perform the action again in the new run

#### Scenario: Cancelled run leaves no middleware state behind
- **WHEN** a run is cancelled after a middleware prepared a notice for the next model request, and the session starts another run
- **THEN** the new run's first request does not contain that notice

### Requirement: Middleware decides only what its stage allows
Each stage of a run SHALL offer middleware a fixed set of decisions. Before a model request: proceed, or stop the run. After a model reply: proceed, stop the run, or discard the reply and retry the request. At the end of a run: finish, continue with another model request, or stop. On a stop: pass it on, or recover with another model request. At any stage a middleware SHALL also be able to queue notices and set generation overrides for the next request.

#### Scenario: Stop before a model request
- **WHEN** a middleware stops the run before a model request
- **THEN** no model request is sent for that step and the run ends with that stop type and message unless a middleware recovers the stop

#### Scenario: Reply discarded and retried by middleware
- **WHEN** a middleware discards a model reply and asks for a retry with a higher output cap
- **THEN** the discarded reply is not stored in history, its token usage is counted, and the next request is sent with that cap

#### Scenario: Continuation at the end of a run
- **WHEN** the model ends its turn and a middleware continues the run with a queued notice
- **THEN** the notice is stored and another model request is sent in the same run

#### Scenario: Stop passed on by every middleware
- **WHEN** a run stops and no middleware recovers the stop
- **THEN** the stop is published with its original type and message

### Requirement: The first decisive middleware wins
Middleware SHALL be consulted in configured order at each stage. The first one that returns a decision other than proceed (or, on a stop, other than pass) SHALL end the stage, and later middleware SHALL NOT be consulted for it. Notices and overrides queued by earlier middleware in that stage SHALL be kept.

#### Scenario: Earlier stop skips later middleware
- **WHEN** the first of two middleware stops the run before a model request
- **THEN** the second middleware is not consulted for that step

#### Scenario: Notices queued before a stop survive a recovery
- **WHEN** one middleware queues a notice, a later middleware stops the run in the same stage, and the stop is recovered
- **THEN** the notice is stored before the next model request

#### Scenario: Notices queued before a published stop are not stored
- **WHEN** one middleware queues a notice, a later middleware stops the run in the same stage, and the stop is published
- **THEN** the notice is not stored in history

### Requirement: Cancellation does not wait for middleware
When a run is cancelled while a middleware is being consulted, the run SHALL end as cancelled without waiting for that middleware, and the decision it would have returned SHALL be discarded.

#### Scenario: Cancel while a middleware is running
- **WHEN** a run is cancelled while a middleware is still deciding before a model request
- **THEN** the run ends with the cancelled outcome and no model request is sent

### Requirement: Context-usage warning is stored once per run
The built-in context middleware (`[[middleware]]` with `type = "context"`) SHALL store one `context_usage` notice in a run, before the first model request at which context usage is at or above `warn_at_percent` of the model's context limit. It SHALL NOT store another one in the same run. `warn_at_percent` is an integer percentage, default 80; `0` turns the warning off; a value not below `compact_at_percent` (default 85) is rejected when the configuration is loaded.

#### Scenario: Warning stored once in a run
- **WHEN** context usage is above the warning threshold for three consecutive model requests of one run
- **THEN** exactly one `context_usage` notice is stored in that run

#### Scenario: Sessions on different models each get one warning
- **WHEN** two sessions on different models run at the same time and both are above their warning threshold for several steps
- **THEN** each run stores exactly one `context_usage` notice

#### Scenario: Next run above the threshold warns again
- **WHEN** a run stored a `context_usage` notice and the session's next run is still above the threshold
- **THEN** the next run stores one `context_usage` notice with the usage at that time

#### Scenario: Usage below the threshold stores no warning
- **WHEN** context usage stays below the warning threshold for a whole run
- **THEN** no `context_usage` notice is stored

#### Scenario: Warning turned off
- **WHEN** `warn_at_percent = 0`
- **THEN** no `context_usage` notice is stored at any usage
