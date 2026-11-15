---
id: agent-config
title: Agent configuration loading and validation
description: How an agent TOML config is parsed, env-interpolated, validated and anchored into a single-agent or quorum Config, and how tool selectors, permissions, prompts and quorum settings are modelled.
tags: config, toml, quorum, permissions, tools, validation
knowledge_paths: ["crates/agent-runtime/src/config/*.rs", "crates/agent-core/src/config/*.rs"]
knowledge_sources: ["crates/agent-runtime/src/config/loader.rs", "crates/agent-runtime/src/config/mod.rs", "crates/agent-runtime/src/config/quorum.rs"]
---

# Agent configuration loading and validation

An agent is configured by one TOML document that becomes a `Config`: either
`Config::Single(SingleAgentConfig)` or `Config::Multi(QuorumConfig)`. User-facing field
documentation is in `docs/docs/agent/configuration.md`; this note explains how loading works
and which checks run, so a change to a field or a check lands in the right place.

## Crate split

- `crates/agent-core/src/config/` holds the data shared with other crates: execution policy
  (`execution.rs`), MCP server and auth settings (`mcp.rs`), managed-workspace settings
  (`workspace.rs`) and `ApprovalNeed` (`approval.rs`).
- `crates/agent-runtime/src/config/` holds the document shapes and the loader: `single.rs`,
  `quorum.rs`, `loader.rs`, `permissions.rs`, `tools.rs`, `prompt.rs`, `mesh.rs`,
  `extensions.rs` (skills, slash commands), `schema.rs` (JSON-schema helpers). `mod.rs`
  re-exports the agent-core types, so callers use `crate::config::*` for both.
- `hooks.rs` only re-exports the hook config types from `crate::hooks::config`.

## Loading pipeline

`load_config(source)` in `loader.rs` takes a `ConfigSource`: `Path` (a file; any `AsRef<Path>`
converts) or `Toml` (an inline string). Steps:

1. Parse the TOML into a `toml::Value`. Comments are gone after this.
2. `interpolate_toml_value` replaces `${VAR}` and `${VAR:-default}` in **string values only**.
   Names match `[A-Z_][A-Z0-9_]*`. An unset variable without a default is an error naming every
   missing variable (`interpolate_env_vars`).
3. `build_config_from_toml_value` removes a top-level `[profile]` table (catalog-only metadata,
   see the agent-profiles note), then discriminates: an `[agent]` table means single; a
   `[quorum]` or `[planner]` table means quorum; anything else is an error.
4. The value is deserialized into the typed struct. Every config struct uses
   `deny_unknown_fields`, so a misspelled key is a load error, not a silent default.
5. Validation (below), then system-prompt resolution, then path anchoring.

File sources resolve prompt files relative to the config file's directory. Inline TOML has no
such directory, so it **rejects** `{ file = ... }` system parts
(`ensure_inline_system_parts`) and only validates template syntax of inline text.

## System prompts

`system` accepts a string or an ordered array mixing inline strings, `{ text, cache }` and
`{ file, cache? }` (`SystemPart`, `deserialize_system_parts` in `prompt.rs`). For file
sources, `resolve_system_parts` reads each file, interpolates env vars in its content,
validates the template (`crate::template::validate_template`) and keeps the cache hint.
After resolution the parts are all inline.

## Validation done at load time

- **MCP servers** (`validate_mcp_servers`): names must be unique within a list, and each server
  passes `agent::mcp::validate_mcp_server`. For a quorum the top-level list and each
  delegate's own list are checked separately.
- **Permissions** (`validate_permissions`): every `permissions.ask` / `always_ask` selector
  must name a configured MCP server (for MCP selectors) and, when `tools` is non-empty, must
  be covered by `tools`, because a tool the agent cannot see can never be approved. The error
  names the agent, field and entry.
- **Peer delegates** (`validate_peer_delegates`): a delegate with `peer` requires
  `[mesh] enabled = true`.
- **Delegate isolation** (`validate_delegate_isolation`): any delegate with
  `isolation = "isolated"` requires `[quorum.workspace] enabled = true`, so the mistake
  surfaces at load rather than at the first delegation.

## Path anchoring

Only for file sources, relative paths are resolved against the config file's directory
(`anchored_at`): `workspace.storage_root` (a leading `~` is left alone for later expansion;
`crates/agent-core/src/config/workspace.rs`) and the `repository_knowledge` `root` and
`allowed_roots` (every relative path is joined, with no `~` handling;
`crates/agent-knowledge/src/settings.rs`). For a single agent the hook base directory is also
set to the config directory. Quorum configs anchor `quorum.workspace` and
`quorum.repository_knowledge`.

## Tool selectors

`ToolSpec` (`tools.rs`) is parsed once from a string: `shell` is `Builtin`, `server.tool` is
`McpSpecific`, `server.*` is `McpAll`. A `*` anywhere else is rejected, as are empty server or
tool segments. A bare name never matches an MCP tool. `ToolSet` is the normalized set
(built-in names plus per-server `All` or `Only`), with `contains` for a concrete `ToolRef` and
`covers` for a selector. `resolve_tools` turns an agent's selectors plus global and delegate
MCP lists into `ResolvedTools` and fails on an unknown built-in or MCP server name.

## Approval permissions

`PermissionsConfig` (`ask`, `always_ask`, `timeout_sec`) converts to the runtime `Permissions`
via `to_permissions`. `Permissions::need(tool)` returns `ApprovalNeed::EveryCall` if the tool
is in `always_ask`, else `Session` if in `ask`, else `None`; hook decisions are merged by the
caller with `ApprovalNeed::stricter`. `Permissions::unmatched` reports `server.tool` entries
whose server is connected but exposes no such tool (typos).

## Quorum shape and defaults

`QuorumConfig` has `quorum` (`QuorumSettings`), `mcp`, `planner`, `delegates`, `mesh`,
`remote_agents` and `dotagents`. `QuorumSettings` defaults: `delegation = true`,
`verification = false`, `delegation_wait_policy = any`, `delegation_wait_timeout_secs = 120`,
`delegation_cancel_grace_secs = 5`, `max_parallel_delegations = 5`; `workspace` and
`repository_knowledge` are disabled unless enabled. How these settings are used at runtime is
in the delegation note. Planner and delegates each carry `tools`, `mode`, `system`,
`parameters`, `middleware`, `execution`, `skills` and `permissions`; a delegate adds `id`,
`description`, `capabilities`, `mcp`, `peer` and `isolation`.

`ExecutionPolicy` (agent-core) groups tool output limits, pruning, compaction, retry, rate
limit and snapshot settings and converts to `RuntimeExecutionPolicy`; what those settings do
is in the execution note.

## Invariants

- Unknown config keys are errors everywhere except the stripped `[profile]` table.
- Inline TOML never reads files.
- Validation failures happen at load time, with the agent and field named.
- `permissions` entries must be selectable by the same agent's `tools`.

Source: `crates/agent-runtime/src/config/loader.rs`, `tools.rs`, `permissions.rs`,
`prompt.rs`, `quorum.rs`, `single.rs`, `mod.rs`; `crates/agent-core/src/config/`.
