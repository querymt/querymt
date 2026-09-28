# Proposal

## Why

QueryMT exposes only a subset of the ACP session state that it already tracks internally, leaving clients without mode/configuration changes, session metadata, context-window usage, plan lifecycle, advisory notices, or compaction progress. In particular, the missing `usage_update` prevents clients from rendering an accurate context meter even though QueryMT records request usage and model context limits.

## What Changes

- Emit stable ACP v1 session updates for current mode, configuration options, session metadata, and context-window usage with cumulative cost when available.
- Upgrade plan reporting to support capability-gated `plan_update` and `plan_removed` notifications while retaining the legacy `plan` fallback for clients that do not advertise plan operations.
- Emit capability-gated ACP v1 Preview notices for advisory runtime events without turning them into conversation history.
- Emit capability-gated ACP v1 Preview compaction lifecycle and summary updates with stable per-compaction identities.
- Preserve stateful update identities and materialized state during session replay, while excluding live-only notices and avoiding replay of transient compaction progress.
- Add translator, capability-negotiation, lifecycle, replay, and serialization tests for every new update path.

## Capabilities

### New Capabilities

- `acp-session-updates`: Defines how QueryMT publishes dynamic session state, usage, plans, notices, and compaction information to ACP v1 clients, including capability gating, fallbacks, and replay semantics.

### Modified Capabilities

None.

## Impact

- Affects ACP connection initialization, protocol type re-exports/features, live event translation, session event definitions, compaction execution, session metadata persistence, and history replay under `crates/agent`.
- Changes the externally observable `session/update` stream for ACP v1 clients; stable updates are additive, while Preview updates are sent only when the client advertises the corresponding capability.
- May require enabling additional `agent-client-protocol` crate features for usage, plan operations, notices, and compaction.
- Does not add ACP v2 support or change the existing prompt lifecycle.
