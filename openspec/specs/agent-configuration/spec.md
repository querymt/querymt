# agent-configuration Specification

## Purpose

Defines how an agent's initial mode and middleware entries become runtime settings, identically for single agents, quorum planners and quorum delegates, and guarantees that invalid values fail the build instead of being ignored.

## Requirements

### Requirement: Initial session mode is a typed agent setting
`[agent]`, `[planner]` and each `[[delegates]]` entry SHALL accept a `mode` key with value `build`, `plan` or `review`, defaulting to `build`. New sessions of that agent SHALL start in the configured mode. An unknown value MUST fail the config load with an error naming the field.

#### Scenario: Single agent starts in plan
- **WHEN** `[agent] mode = "plan"` and a new session is created
- **THEN** the session's mode is `plan`

#### Scenario: Delegate starts in review
- **WHEN** a delegate sets `mode = "review"` and the planner delegates to it
- **THEN** the delegate's session starts in `review`

#### Scenario: Mode typo in a quorum
- **WHEN** a quorum planner or delegate sets `mode = "plna"`
- **THEN** loading fails with an error naming the `mode` field and the valid values

### Requirement: Agent mode middleware only configures reminders
The `agent_mode` middleware entry SHALL configure mode reminders only and MUST reject unknown keys, including the former `default` key.

#### Scenario: Leftover default key
- **WHEN** an `agent_mode` middleware entry sets `default = "plan"`
- **THEN** building the agent fails with an error naming the unknown `default` key

### Requirement: Middleware errors fail the build
Building a single agent, quorum planner or quorum delegate SHALL fail when a configured middleware entry has an unknown type or invalid configuration. A middleware entry that is explicitly turned off by its own configuration SHALL be skipped without error. No failure MAY be skipped based on the text of its error message.

#### Scenario: Invalid delegate middleware
- **WHEN** a quorum delegate configures a `limits` middleware with an invalid value
- **THEN** building the quorum fails with an error naming the delegate and the middleware

#### Scenario: Middleware turned off
- **WHEN** a `context` middleware entry is configured with its enable flag off
- **THEN** the agent builds and that middleware is not installed

#### Scenario: Same behavior for single agent and quorum
- **WHEN** the same invalid middleware entry is used in a single-agent config and in a quorum planner config
- **THEN** both builds fail with the same error
