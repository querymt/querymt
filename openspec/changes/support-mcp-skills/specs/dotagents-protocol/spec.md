# Spec Delta

## ADDED Requirements

### Requirement: Protocol delegation targets are offered skills
A materialized protocol delegation target SHALL use the skills configuration of the runtime that registers it: the standalone agent's skills configuration, or the quorum planner's for a quorum. When skills are enabled for that runtime and the target's `tools` restriction is absent or includes `skill`, the target SHALL offer the skill tool. Its local skills SHALL come from its own session workspace, falling back to the registering runtime's project root when the session has none.

#### Scenario: Unrestricted target gets skills
- **WHEN** a standalone agent with skills enabled delegates to an internal protocol target whose `config.json` has no `tools` restriction
- **THEN** the target's session offers the skill tool listing the skills of its workspace

#### Scenario: Restricted target without skill
- **WHEN** a protocol target's `config.json` sets `tools` without `skill`
- **THEN** the target's session does not offer the skill tool

#### Scenario: Parent skills disabled
- **WHEN** the registering runtime has skills disabled or not configured
- **THEN** no protocol target offers the skill tool
- **AND** protocol loading does not enable skills

#### Scenario: Quorum target uses the planner's skills configuration
- **WHEN** a quorum profile with delegation enabled registers a protocol target and its planner has skills enabled with custom permissions
- **THEN** the target offers the skill tool and applies the planner's skill permissions

### Requirement: Protocol targets do not wait for skill approval
A protocol delegation target SHALL NOT request user approval to load a skill. WHEN a skill's permission in a target would require approval, THEN the load SHALL fail immediately with a permission error that names the skill and the permission pattern that would allow it.

#### Scenario: Unconfigured server skill in a target
- **WHEN** a target attached to server `docs` loads `docs.release-notes` and no permission pattern names `docs`
- **THEN** the load fails at once with a permission error naming `docs.release-notes` and the pattern `docs.*`
- **AND** no approval request is sent and the delegated turn continues

#### Scenario: Explicitly allowed server skill in a target
- **WHEN** the registering runtime's skill permissions contain `"docs.*" = "allow"` and a target attached to server `docs` loads `docs.release-notes`
- **THEN** the skill loads without an approval request

### Requirement: Protocol targets are offered only their own MCP servers' skills
A protocol delegation target SHALL be offered MCP skills only from the MCP servers it attaches, which are the servers named by its `config.json` `mcpServers` or, when that field is absent, every enabled manifest server. Skills of the parent's other servers SHALL NOT be offered to the target.

#### Scenario: Restricted MCP servers
- **WHEN** the manifest defines servers `docs` and `ops` that both publish skills and a target's `config.json` sets `mcpServers: ["docs"]`
- **THEN** the target's skill tool lists `docs` skills and no `ops` skills

#### Scenario: Server-qualified tool restriction
- **WHEN** a target sets `tools: ["skill", "docs.*"]` and attaches server `docs`
- **THEN** the target's MCP tool policy admits `docs`, and `docs` tools and skills are offered to it

### Requirement: Protocol targets that attach MCP servers can reach them
WHEN a protocol target with a `tools` restriction attaches MCP servers, or its `tools` list contains a server-qualified entry or an attached server's name, THEN its tool policy SHALL admit MCP tools so that the allowlisted MCP tools and skills reach the model. Its `tools` list SHALL remain the allowlist. A target without a `tools` restriction SHALL keep the default policy. The policy SHALL be inferred by the same rule as for standalone agents and configured quorum members.

#### Scenario: Restricted target with MCP servers
- **WHEN** a target sets `tools: ["read_tool", "docs.*"]` and attaches server `docs`
- **THEN** the target is offered `read_tool` and `docs` MCP tools
- **AND** no other built-in tools are offered

#### Scenario: Restricted target without MCP servers
- **WHEN** a target sets `tools: ["read_tool"]` and attaches no MCP servers
- **THEN** the target is offered only `read_tool` and no MCP tools
