# Spec Delta

## ADDED Requirements

### Requirement: Scheduled runs record a run start
A run started by a schedule SHALL record one durable run-start event with `origin` set to `scheduled`. The event SHALL be recorded after the run lease is acquired and before any other event of that run, and SHALL carry the same `run_id` as the run's completion event. A scheduled execution that is rejected before a run exists SHALL record no run-start event.

#### Scenario: Scheduled run has a start and a completion
- **WHEN** a schedule fires for an idle local session and the run completes
- **THEN** the journal contains one run-start event with `origin` `scheduled` and one run-completion event with the same `run_id`
- **AND** the run-start event precedes every other event of that run

#### Scenario: Interactive run start is unchanged
- **WHEN** a user prompt starts a run
- **THEN** the run-start event has `origin` `interactive`

#### Scenario: Queued prompt run start is unchanged
- **WHEN** a queued prompt starts after the previous run completes
- **THEN** the journal contains a run-start event with `origin` `interactive` followed by the queued-input-started event for that input, both with the new run's `run_id`

#### Scenario: Scheduled execution rejected while a run is active
- **WHEN** a schedule fires for a session that already has an active run
- **THEN** the execution is rejected and no run-start event is recorded for it

#### Scenario: Scheduled execution rejected by the run lease
- **WHEN** a schedule fires for a session whose run lease is held by another process
- **THEN** a scheduled-execution-failed event is recorded and no run-start event is recorded

#### Scenario: Crashed scheduled run is reconciled
- **WHEN** a process dies during a scheduled run and the session is later loaded by a live process
- **THEN** the journal gains one run-completion event with outcome `interrupted` for that run's `run_id`
