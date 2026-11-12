# Spec Delta

## ADDED Requirements

### Requirement: A run has one identifier
Every local prompt run SHALL have exactly one identifier, a lowercase hyphenated UUID. Its run-start and run-completion events, steering results, hook payloads and turn snapshot records SHALL all carry that same value, whether the field is named `run_id` or `turn_id`.

#### Scenario: Run events, hooks and snapshot records share the run id
- **WHEN** a prompt run starts, fires a pre-tool-use hook, changes a file and completes
- **THEN** the run-start event's `run_id`, the hook input's `turn_id`, the turn snapshot start and change records' `turn_id`, and the run-completion event's `run_id` are equal

#### Scenario: Steering reports the active run's id
- **WHEN** input is steered into an active run
- **THEN** the steering result's `run_id` equals that run's run-start event `run_id`

### Requirement: Hook payloads never carry an empty turn id
Every hook command input SHALL carry a non-empty `turn_id` that parses as a UUID. Hooks fired by work that runs outside a prompt run, such as a native `/compact` command, SHALL receive a turn id minted for that work.

#### Scenario: Hook fired inside a run gets the run's id
- **WHEN** a stop hook fires at the end of a prompt run
- **THEN** its input `turn_id` equals that run's `run_id`

#### Scenario: Hook fired outside a run gets a fresh id
- **WHEN** a compaction hook fires because of a native `/compact` command with no active run
- **THEN** its input `turn_id` is a non-empty UUID string
