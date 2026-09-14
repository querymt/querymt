## Why

QueryMT currently discovers skills under `.agents/skills`, but it cannot consume the rest of a portable `.agents/` configuration. Supporting the draft .agents Protocol lets users share instructions, prompts, MCP servers, model presets, sub-agents, repeat tasks, and memories using the agent crate's existing runtime facilities instead of duplicating that configuration in QueryMT-specific files.

## What Changes

- Add opt-in discovery of global `~/.agents/` and workspace `<project>/.agents/` layers, with deterministic overlay semantics and no implicit creation of a workspace directory.
- Parse and resolve `agents.md`, `system-prompt.md`, `mcp.json`, `models.json`, `skills/`, `agents/`, `tasks/`, and `memories/` into a typed, inspectable manifest with source-aware diagnostics.
- Compose protocol instructions and system prompts into single-agent and quorum runtimes while preserving explicit QueryMT configuration.
- Adapt protocol MCP `stdio` and `streamable-http` servers and model presets to QueryMT's existing MCP, LLM, and profile configuration; reject WebSocket and unknown MCP transports with diagnostics until QueryMT supports them.
- Extend existing skill discovery for protocol-compatible metadata and filename casing without regressing current `.skills`, `.claude/skills`, `.qmt/skills`, or configured sources.
- Materialize supported sub-agent profiles as delegation targets using the current agent registry and profile/runtime builders: standalone agents gain registry targets, quorum profiles gain additional delegates, and explicit QueryMT delegates win ID collisions with protocol targets.
- Reconcile enabled repeat tasks with the existing durable task and scheduler services, including interval and startup behavior, but require explicit fingerprint-bound trust before workspace tasks can be persisted, scheduled, or run.
- Import memory documents idempotently into the existing knowledge store with stable source identities.
- Reject unsafe paths and unsupported runtime mappings with actionable diagnostics; malformed entries do not hide valid siblings.
- Document the supported protocol subset. Layout preferences, SpeakMCP settings, backup/write-back behavior, Hub publishing/installing, and unsupported external sub-agent transports remain out of scope.

## Capabilities

### New Capabilities

- `dotagents-protocol`: Discovery, parsing, layering, runtime adaptation, reconciliation, diagnostics, and security behavior for the supported `.agents/` protocol files.

### Modified Capabilities

None. This repository has no existing OpenSpec capabilities; current QueryMT behavior is preserved as compatibility requirements in the new capability.

## Impact

- Affected code: `crates/agent` configuration loading, API builders, profile catalogs, skills, MCP setup, delegation registry, task scheduler, and knowledge ingestion.
- Public API: new protocol loader/resolver types and builder/config entry points for enabling and inspecting `.agents` support.
- Runtime behavior: enabled protocol files can augment agent startup and workspace-specific sessions; absence of `.agents/` leaves behavior unchanged.
- Persistence: imported tasks and memories require deterministic identities and reconciliation against existing storage to avoid duplicates across restarts.
- Security: provider secrets, MCP environment values, executable sub-agent connections, symlinks, path traversal, and repository-supplied recurring tasks require explicit validation, redacted diagnostics, and workspace-task trust controls.
- Dependencies: reuse the existing JSON, serde, frontmatter, filesystem notification, RMCP, scheduler, profile, and knowledge infrastructure. The evaluated `dotagents` crate is an unrelated binary-only `.dotagents/` deployment tool, and `agent-runbooks` only initializes `.agent/runbooks/`; neither supplies reusable `.agents` Protocol parsing or runtime integration, so neither is added.
