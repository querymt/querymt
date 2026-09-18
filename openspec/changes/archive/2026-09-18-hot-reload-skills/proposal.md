# Hot-Reload Skills Without Agent Restart

## Why

The skill registry is snapshotted once at agent construction: skills added to or removed from `.agents/skills`, `.qmt/skills`, or any other discovery source after startup are invisible to the model, and deleted skills linger as stale entries in the `skill` tool's enum. Today the only remedy is restarting the whole agent, which loses session state and interrupts workflows.

## What Changes

- `SkillRegistry` gains a transactional replace-semantics reload path (`reload_from_sources`) that drops skills whose source files were removed without exposing a partial or empty registry when source traversal fails.
- Refresh discovery gains a strict failure mode for source traversal while retaining the current non-fatal handling of absent sources and malformed individual skill definitions.
- `SkillTool` retains its discovery configuration (search paths + `include_external`) after construction instead of discarding it, so it can re-discover on demand.
- The `skill` tool refreshes whenever a model-facing tool-schema snapshot is generated, so added, removed, and edited skills are reflected in the next turn's schema without a restart.
- Loading a skill that is not in the registry triggers one reload-and-retry before failing, and the not-found error reports the currently registered callable skill IDs, including the empty-set case.
- Protocol skills are registered, advertised, and invoked consistently by stable effective ID (explicit `id`, otherwise the existing fallback), while their human-readable names remain display metadata.
- No change to discovery sources, precedence, `.agents/skills` special handling (recursive walk, `skill.md` spelling, `enabled` metadata), or permission policy.

## Capabilities

### New Capabilities
- `agent-skills`: Discovery, loading, and lifecycle of agent skills from global/project sources, including freshness guarantees (skills reflect filesystem state at turn boundaries without an agent restart).

### Modified Capabilities

<!-- None: openspec/specs/ is empty; agent-skills is the first spec for this subsystem. -->

## Impact

- **Code**: `crates/agent/src/skills/discovery.rs` (strict refresh discovery path), `crates/agent/src/skills/registry.rs` (transactional reload and effective-ID keys), `crates/agent/src/skills/tool.rs` (retain sources, refresh in `definition()`, reload-on-miss in `call()`), `crates/agent/src/skills/mod.rs` (`build_skill_tool` passes config through).
- **Behavior**: Model-visible `skill` tool schemas update before model requests; intra-turn invocation of an unadvertised new skill is covered by reload-on-miss. Quorum delegate/planner skill tools inherit the behavior automatically because they share `build_skill_tool`.
- **Dependencies**: None added.
- **Out of scope**: Slash-command hot-reload (its `reload()` API already exists but has no production wiring in this workspace); file-watcher or actor-based live refresh.
