# Proposal

## Why

QueryMT can refresh tools exposed by MCP servers, but its general tool set is otherwise fixed when the agent configuration is built. Extensions such as pi-lens need to register tools once and activate or deactivate selected tools between model turns to control prompt size without restarting the session.

## What Changes

- Introduce a session-scoped dynamic tool catalog that separates tool registration from model-visible activation.
- Allow authorized runtime integrations to register, unregister, activate, and deactivate tools at safe turn boundaries.
- Apply existing allow/deny policy and collision rules to dynamically registered tools.
- Publish deterministic tool-set changes and refresh model tool definitions on the next model request.
- Unify built-in/provider dynamic activation semantics with the existing MCP `tools/list_changed` snapshot behavior without changing the MCP protocol.
- Ensure in-flight tool calls resolve against the generation from which they were selected and are not invalidated by concurrent activation changes.

## Capabilities

### New Capabilities

- `dynamic-tool-availability`: Defines session-scoped tool registration, activation, deactivation, policy enforcement, collision handling, and turn-boundary visibility.

### Modified Capabilities

None.

## Impact

- Affects `ToolRegistry`, `AgentConfig::collect_tools`, and per-session runtime tool state in `crates/agent/src/`.
- Generalizes the atomic snapshot approach already used for MCP tools.
- Adds agent events/API operations for observing and controlling active tools.
- Requires tests across built-in, provider, MCP, and dynamically registered tool sources.
- Does not require JavaScript execution or Pi extension compatibility.
