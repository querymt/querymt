# Spec Delta

## ADDED Requirements

### Requirement: Delegates are configured under the agent
An `[agent]` configuration SHALL accept `[[delegates]]`, an array of tables with the same keys as a delegate of a `[quorum]` configuration. The default SHALL be an empty array. Each entry SHALL become a roster member with origin `config`. An unknown key in an entry SHALL be a load error. Two entries with the same `id` SHALL be a load error naming the ID. An entry with `isolation = "isolated"` SHALL be a load error unless `[agent.workspace] enabled = true`.

#### Scenario: Agent configuration with delegates loads
- **WHEN** a configuration has `[agent]` and two `[[delegates]]` entries `coder` and `reviewer`
- **THEN** the configuration loads
- **AND** the runtime's roster lists `coder` and `reviewer` with origin `config`

#### Scenario: Unknown delegate key is rejected
- **WHEN** a `[[delegates]]` entry under an `[agent]` configuration contains a key that is not a delegate setting
- **THEN** loading fails with an error naming the key

#### Scenario: Duplicate delegate ID is rejected
- **WHEN** two `[[delegates]]` entries have `id = "coder"`
- **THEN** loading fails with an error naming `coder`

#### Scenario: Isolated delegate without the workspace service is rejected
- **WHEN** a `[[delegates]]` entry sets `isolation = "isolated"` and `[agent.workspace] enabled` is false or absent
- **THEN** loading fails with an error naming the delegate and `[agent.workspace]`

### Requirement: Delegation wait settings are configured under the agent
`[agent.delegation]` SHALL accept `wait_policy` (`"any"` or `"all"`, default `"any"`), `wait_timeout_secs` (non-negative integer, seconds, default 120; 0 means no timeout) and `cancel_grace_secs` (non-negative integer, seconds, default 5). They SHALL apply to the root agent's delegations. An unknown key in the table or an invalid value SHALL be a load error naming the key.

#### Scenario: Delegation settings default when omitted
- **WHEN** an `[agent]` configuration has no `[agent.delegation]` table
- **THEN** the configuration loads with `wait_policy = "any"`, `wait_timeout_secs = 120`, `cancel_grace_secs = 5`, `max_parallel = 5`, `verification = false` and no summary

#### Scenario: Invalid delegation settings are rejected
- **WHEN** `[agent.delegation]` sets `wait_policy = "first"`, or a negative `wait_timeout_secs`, or a key that is not a delegation setting
- **THEN** loading fails with an error naming the offending key

### Requirement: Delegation limits, verification and summary are configured under the agent
`[agent.delegation]` SHALL accept `max_parallel` (integer of at least 1, default 5), `verification` (boolean, default false) and `summary` (a table with the keys of `[quorum.delegation_summary]`; default absent, meaning no planning summary). An invalid value SHALL be a load error naming the key.

#### Scenario: Delegation settings accept valid values
- **WHEN** `[agent.delegation]` sets `wait_policy = "all"`, `wait_timeout_secs = 0`, `cancel_grace_secs = 10`, `max_parallel = 2` and `verification = true`, and `[agent.delegation.summary]` sets a provider and model
- **THEN** the configuration loads with exactly those values

#### Scenario: Zero parallel delegations is rejected
- **WHEN** `[agent.delegation]` sets `max_parallel = 0`
- **THEN** loading fails with an error naming `max_parallel`

### Requirement: Snapshot policy is configured under the agent
`[agent]` SHALL accept `snapshot_policy` (`"none"`, `"metadata"` or `"diff"`). When absent, the policy SHALL be `diff`, as before. The root agent and every delegate SHALL use it. Any other value SHALL be a load error naming `snapshot_policy`.

#### Scenario: Snapshot policy can be turned off
- **WHEN** an `[agent]` configuration sets `snapshot_policy = "none"`
- **THEN** the runtime's root agent and its delegates use snapshot policy `none`

#### Scenario: Invalid snapshot policy is rejected
- **WHEN** `[agent]` sets `snapshot_policy = "sometimes"`
- **THEN** loading fails with an error naming `snapshot_policy`

### Requirement: Quorum configuration is deprecated
A configuration written with `[quorum]` and `[planner]` SHALL still load and SHALL produce the same runtime as before: the planner as the root agent and the delegates as roster members. Loading it SHALL emit one deprecation notice that names `[agent]`, `[[delegates]]` and `[agent.delegation]` as the replacement. A file that contains `[agent]` together with `[quorum]` or `[planner]` SHALL be a load error.

#### Scenario: Quorum configuration loads with a deprecation notice
- **WHEN** a configuration with `[quorum]`, `[planner]` and one `[[delegates]]` entry is loaded
- **THEN** the runtime's root agent uses the planner's model, tools and system prompt, and the roster lists the delegate
- **AND** exactly one deprecation notice naming `[agent]` is emitted

#### Scenario: Agent and quorum tables cannot be mixed
- **WHEN** a configuration contains both `[agent]` and `[quorum]`
- **THEN** loading fails with an error naming the unexpected table
