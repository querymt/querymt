# Spec Delta

## MODIFIED Requirements

### Requirement: Supported sub-agents extend the selected runtime's delegation registry
The system SHALL parse `.agents/agents/<entry>/agent.md` metadata and body, apply supported fields from adjacent `config.json`, and make each enabled, supported profile available as a delegation target only when delegation is enabled for the selected QueryMT runtime. Protocol targets SHALL be added to the runtime's roster next to explicitly configured members, for every configuration shape, without replacing the root agent or any configured member. Delegating to a protocol target SHALL run the delegated task. A protocol target's runtime SHALL be built when it is first delegated to, not when the runtime starts. The profile body SHALL become that target's agent-specific system prompt. Internal connections SHALL reuse QueryMT's local runtime, model, tool, MCP, and registry facilities. MCP transport support for a sub-agent is distinct from the sub-agent's own ACP or executable connection type.

#### Scenario: Internal sub-agent extends a standalone agent
- **WHEN** a standalone agent has delegation enabled and an enabled protocol profile has role `delegation-target` with a supported internal connection
- **THEN** it appears in the standalone agent's registry with its ID, name, description, and capabilities
- **AND** delegating to it starts or reuses the configured local runtime, runs the task, and returns the result to the standalone agent

#### Scenario: Internal sub-agent extends a quorum profile
- **WHEN** a runtime with configured delegates, written as `[agent]` with `[[delegates]]` or as the deprecated `[quorum]`, has delegation enabled and resolves an enabled supported protocol target whose ID is not explicitly configured
- **THEN** the target is added as an additional member
- **AND** the root agent and the configured delegates remain unchanged

#### Scenario: Runtime with configured delegates and a protocol target starts
- **WHEN** a configuration defines at least one delegate, delegation is enabled, and the protocol directory defines an enabled supported delegation target
- **THEN** the runtime starts without error
- **AND** both the configured delegate and the protocol target are listed in the roster

#### Scenario: Protocol target is built on first delegation
- **WHEN** a runtime with a protocol delegation target has started and no task has been delegated to that target
- **THEN** the target's runtime has not been built
- **AND** the first delegation to the target builds it, and later delegations reuse it

#### Scenario: Delegation is disabled
- **WHEN** protocol targets are resolved for a standalone or quorum runtime with delegation disabled
- **THEN** the targets remain inspectable in the manifest
- **AND** protocol loading does not enable delegation or register active targets

#### Scenario: Explicit delegate collides with a protocol target
- **WHEN** an explicitly configured QueryMT standalone registry target or quorum delegate has the same normalized ID as a resolved protocol target
- **THEN** the explicitly configured target wins
- **AND** the protocol target is not registered
- **AND** a diagnostic identifies the collision and both sources

#### Scenario: Profile is disabled
- **WHEN** a profile has `enabled: false`
- **THEN** it is not registered as a delegation target

#### Scenario: External connection cannot be represented safely
- **WHEN** a profile requests an unsupported executable or connection type
- **THEN** no process is launched
- **AND** a diagnostic identifies the unsupported connection
