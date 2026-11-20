# Spec Delta

## Purpose

Defines how long the agent waits before it retries a failed model request, so that a dropped connection is retried quickly while an overloaded or rate-limited provider is given time to recover.

## ADDED Requirements

### Requirement: Transport failures are retried after a short wait
When a model request fails without a response from the provider (connection refused or reset, DNS failure, timeout, connection closed, or a response stream that breaks or cannot be decoded) and attempts remain, the agent SHALL wait `transport_wait_secs` multiplied by `backoff_multiplier` once for each earlier retry of that request, with the configured jitter, and never longer than `max_wait_secs`. `default_wait_secs` SHALL NOT apply to these failures.

#### Scenario: First retry after a network timeout
- **WHEN** a model request fails with a network timeout, `transport_wait_secs = 2` and `backoff_multiplier = 2.0`
- **THEN** the retry-wait event reports a wait of about 2 seconds, not about 60

#### Scenario: Second retry backs off
- **WHEN** the retried request fails again with a connection reset
- **THEN** the second retry-wait event reports a wait of about 4 seconds

#### Scenario: Stream breaks after partial output
- **WHEN** a streaming response stops with a body decoding error after some output arrived
- **THEN** the request is retried after about `transport_wait_secs`

#### Scenario: Immediate retry
- **WHEN** `transport_wait_secs = 0` and a model request fails with a connection failure
- **THEN** the request is retried without waiting

### Requirement: Provider-reported failures keep the provider wait
When the provider answers a model request with a retryable failure (overloaded, rate limited, or an unclassified transient error), the agent SHALL wait for the delay the provider supplied, capped at `max_wait_secs`. When the provider supplied none, it SHALL wait `default_wait_secs` multiplied by `backoff_multiplier` once for each earlier retry, with the configured jitter, capped at `max_wait_secs`. `transport_wait_secs` SHALL NOT apply to these failures.

#### Scenario: Overloaded without a provider delay
- **WHEN** the provider answers with an overloaded error and no retry delay, and `default_wait_secs = 60`
- **THEN** the retry-wait event reports a wait of about 60 seconds

#### Scenario: Provider supplies a delay
- **WHEN** the provider answers with a rate-limit error and a retry delay of 20 seconds
- **THEN** the retry-wait event reports a wait of 20 seconds

#### Scenario: Provider delay above the cap
- **WHEN** the provider supplies a retry delay of 900 seconds and `max_wait_secs = 300`
- **THEN** the retry-wait event reports a wait of 300 seconds

### Requirement: Transport retries share the attempt budget
Retries after transport failures and retries after provider-reported failures SHALL count against the same per-request attempt budget (`max_retries`). A failure that is not retryable SHALL end the request without a wait, whatever its kind.

#### Scenario: Mixed failures exhaust one budget
- **WHEN** `max_retries = 3` and a request fails with a timeout, then with an overloaded error, then with a timeout
- **THEN** the request is not attempted a fourth time and the run reports the last failure

#### Scenario: Peer version mismatch is not retried
- **WHEN** a request to a mesh peer fails because the peers run incompatible protocol versions
- **THEN** the request fails without a retry wait

### Requirement: The transport wait setting is validated
`[agent.execution.rate_limit]` SHALL accept `transport_wait_secs` (integer seconds, 0 to 300, default 10). A value outside the range or of the wrong type SHALL be a load error naming `transport_wait_secs`.

#### Scenario: Transport wait defaults when omitted
- **WHEN** a configuration does not set `transport_wait_secs`
- **THEN** it loads with `transport_wait_secs = 10`

#### Scenario: Out-of-range transport wait is rejected
- **WHEN** `[agent.execution.rate_limit]` sets `transport_wait_secs = 900` or `transport_wait_secs = -1`
- **THEN** loading fails with an error naming `transport_wait_secs`
