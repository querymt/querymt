# session-compaction Specification

## Purpose

Defines how the request that summarizes a session's history is sent: which failures are retried and under which limits, how its attempts are reported, and how it is cancelled.

## Requirements

### Requirement: Provider errors during compaction are retried like a model request's
A provider error during a compaction request SHALL be handled by the rules of a model request: it is retried only when the error is retryable, after the wait that the error class calls for, and for at most the number of attempts that `[agent.execution.rate_limit]` `max_retries` allows. The wait and the resumption SHALL be reported to clients with the events a model request uses. `[agent.execution.compaction.retry]` SHALL NOT limit or time these retries.

#### Scenario: Overloaded provider during compaction
- **WHEN** the compaction request fails with an overloaded error that names a 2 second delay, and the next attempt succeeds
- **THEN** the second attempt is sent after about 2 seconds and the summary is stored

#### Scenario: Connection failure during compaction
- **WHEN** the compaction request fails because the connection was refused, and the next attempt succeeds
- **THEN** the second attempt is sent after the wait `[agent.execution.rate_limit]` `transport_wait_secs` gives, not after `[agent.execution.compaction.retry]` `initial_backoff_ms`

#### Scenario: Rejected credential during compaction
- **WHEN** the compaction request fails with an authentication error
- **THEN** the compaction fails after that one request

#### Scenario: Provider errors on every compaction attempt
- **WHEN** every attempt of the compaction request fails with a retryable provider error
- **THEN** the compaction fails after the attempts `[agent.execution.rate_limit]` `max_retries` allows
- **AND** the stored history is unchanged

#### Scenario: Provider error with the summary budget at zero
- **WHEN** `[agent.execution.compaction.retry]` `max_retries` is `0`, `[agent.execution.rate_limit]` `max_retries` allows a second request, and the compaction request fails once with a retryable provider error before the second request returns a valid summary
- **THEN** the request is retried and the summary of the second request is stored

#### Scenario: Retry wait during compaction is reported
- **WHEN** a compaction request waits before a retry
- **THEN** clients receive the same wait event and resume event as for a model request that waits

### Requirement: A compaction retry wait can be cancelled
A run that is cancelled while a compaction request waits to retry SHALL stop waiting, send no further compaction request and leave the stored history unchanged.

#### Scenario: Cancelled while compaction waits to retry
- **WHEN** the run is cancelled during the wait before a compaction retry
- **THEN** no further compaction request is sent and no summary is stored

### Requirement: A compaction request timeout is not retried
A compaction request that ends in a request timeout SHALL fail the compaction without another attempt, because the remote side may still be working on the request.

#### Scenario: Timeout during compaction
- **WHEN** the compaction request times out
- **THEN** the compaction fails after that one request with an error that names the timeout

### Requirement: Rejected summaries are retried within the compaction retry budget
A reply the agent rejects as a summary (empty, refused or cut off) SHALL be retried up to `[agent.execution.compaction.retry]` `max_retries` times (integer, 0 or more, default `3`). The wait before the first retry SHALL be `initial_backoff_ms` milliseconds (integer, 0 or more, default `1000`), multiplied by `backoff_multiplier` (number, default `2.0`) before each further retry. Parsing and validation of the three keys are unchanged.

#### Scenario: Empty summary is retried
- **WHEN** the first compaction reply is empty and the second is a valid summary
- **THEN** the second request is sent after `initial_backoff_ms` and its summary is stored

#### Scenario: Summary rejected on every attempt
- **WHEN** every compaction reply is rejected and `max_retries` is `1`
- **THEN** the compaction fails after two requests and the stored history is unchanged

#### Scenario: Each summary attempt has its own provider retries
- **WHEN** a compaction reply is rejected, and the request that follows fails once with a retryable provider error before it succeeds
- **THEN** the provider error is retried by the rules of a model request and the summary of the successful attempt is stored

### Requirement: Compaction request spans report their attempts
The span of a compaction request SHALL carry the attribute `gen_ai.request.attempt` with the number of the last attempt started, and one event named `gen_ai.request.attempt` for each attempt that ended. The event of a failed attempt SHALL carry `error.type` and SHALL NOT carry the error message.

#### Scenario: Compaction request retried once
- **WHEN** a compaction request fails with an overloaded error and its second attempt succeeds
- **THEN** its span has `gen_ai.request.attempt = 2` and two attempt events, the first with `error.type`

#### Scenario: Compaction request without a retry
- **WHEN** a compaction request succeeds on its first attempt
- **THEN** its span has `gen_ai.request.attempt = 1` and one attempt event without `error.type`

#### Scenario: Rejected summary attempt
- **WHEN** a compaction reply is rejected as a summary and the next reply is a valid summary
- **THEN** the first attempt event carries `error.type = output_rejected` and the second carries no `error.type`
