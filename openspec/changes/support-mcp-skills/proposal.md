# Proposal

## Why

rmcp 3.9.0 implements the MCP Skills extension (SEP-2640, `io.modelcontextprotocol/skills`). With it, MCP servers can publish Agent Skills (`SKILL.md` plus supporting files, each with a SHA-256 manifest) through `skills/list`, `skills/get` and `resources/read`. QueryMT already connects MCP servers per session: agent config, `.agents/mcp.json`, ACP clients and sub-agents. It also already has a `skill` tool. But it cannot offer skills that those servers publish, and its skill tool is built once per agent. Because of that, it cannot reflect what a particular session, or a delegated sub-agent, is actually connected to.

## What Changes

- Raise the rmcp requirement from `3.8` to `3.9` in every manifest that depends on it. `Cargo.lock` is already at 3.9.0.
- During connection, list the skills of every MCP server that declares the skills extension, and refresh them when that server announces a resource list change (including under the 2026-07-28 protocol) or when a load misses. Servers that do not declare the extension receive no skills requests. Servers whose names contain `.` contribute no skills, so callable IDs stay unambiguous. A skills failure never fails the server connection.
- Apply MCP tool and skill refreshes atomically, so concurrent refreshes cannot lose updates.
- Offer MCP skills through the **single existing `skill` tool**, under server-qualified callable IDs (`<server>.<skill>`). No second skill tool is added.
- Make the `skill` tool **session-owned**:
  - Its schema is built from the session's own workspace and its own MCP connections.
  - Per-session MCP servers (ACP clients, sub-agents) therefore only expose their own skills.
  - The workspace-aware special case on the tool trait is removed.
- Verify MCP skill content against the server's manifest (size and `sha256` digest), re-validate the entry at load time, and check the frontmatter name before the content reaches the model. On a mismatch the skill is rejected.
- Treat server text as untrusted: sanitize and cap listed descriptions, cap content at 256 KiB, and escape skill output so it cannot break out of the `<skill_content>` wrapper.
- Add an optional `file` argument to `skill` for loading a skill's supporting files:
  - Paths are normalized first; absolute, scheme-qualified, query/fragment and (percent-encoded) traversal paths are rejected.
  - Local skills read files confined to the skill directory.
  - MCP skills read files from the skill's manifest, with verification.
- MCP skills require user approval by default (`Ask`). Only explicit permission patterns that name the server or skill (`server.*`, `server.skill`) change that. A catch-all `"*" = "deny"` still denies them; a catch-all `allow`/`ask` does not admit them.
- Skill permission wildcards resolve deterministically (longest prefix wins) for local and MCP skills.
- Approval prompts name the server and skill and use a per-call ID, so concurrent loads do not collide.
- MCP skills are visible only when the session's tool configuration admits that server: policy includes built-in and MCP tools, the allowlist (when set) contains `server.*`, and the denylist contains neither `server.*` nor `server`.
- Add a `skills.mcp` setting (default `true`) that turns off MCP skills while keeping local skills.
- **Sub-agents gain skills:**
  - `.agents` protocol delegation targets inherit the parent runtime's skill configuration. They are offered local skills for their own workspace plus the skills of only their own attached MCP servers, gated by their `tools` restriction.
  - Protocol targets cannot show approval prompts to the user, so they never ask: a load that would need approval fails at once with an error naming the pattern to configure.
  - Protocol targets that attach MCP servers infer their tool policy the same way standalone agents do, through one shared helper that replaces the three existing copies, so restricted targets can reach their selected servers.
  - Configured quorum delegates and the planner keep their per-agent skill configuration and automatically see their own MCP servers' skills.
- **BREAKING (Rust API):** `Tool::definition_for_cwd`, `ToolRegistry::definitions_for_cwd`/`definition_for_cwd` and the public `SkillTool` constructors are removed or replaced. Agent-level skill setup moves to a skill catalog held by the agent configuration. TOML configuration stays compatible; the new key is additive.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `agent-skills`: changes how skills are offered and loaded:
  - MCP skills are offered through the skill tool, with server-qualified IDs, local-wins collisions, verification, bounded and escaped content, supporting-file loading, approval by default, visibility tied to MCP tool access, refresh on load misses, and a `skills.mcp` switch.
  - Permission wildcards resolve deterministically for all skills.
  - Skill availability becomes session-scoped, which also applies to delegated agents.
- `mcp-server-connection`: skills are listed from eligible servers that declare the skills extension, failures do not fail the connection, the list is refreshed on resource list changes (also under 2026-07-28), and concurrent refreshes do not lose updates.
- `dotagents-protocol`: protocol delegation targets inherit skill configuration, are offered only their own MCP servers' skills, and fail fast instead of waiting for approval. Targets that attach MCP servers can reach them under a tool restriction.

## Impact

- **Dependencies:** `rmcp` `3.9` in `crates/agent/Cargo.toml`, `crates/querymt/Cargo.toml` and `crates/querymt-mobile-ffi/Cargo.toml`. No new crates.
- **Code (`crates/agent`):**
  - `skills/`: catalog, session-bound tool, parser string entry point, permissions, output escaping and limits.
  - `tools/`: tool trait, registry, session-aware lookup.
  - `agent/tools.rs`: server-visibility and tool-policy inference helpers.
  - `agent/`: `SessionRuntime`, MCP snapshot, connect/merge in `protocol.rs`, tool collection and call paths in `agent_config.rs`, `execution/tool_calls.rs` and `execution/transitions.rs`, session materialization.
  - `elicitation.rs`: list-changed handling and atomic snapshot replacement.
  - `api/agent.rs`, `api/quorum.rs`, `api/dotagents_targets.rs`, `config/extensions.rs`.
  - `test_utils/mcp_harness.rs`.
- **APIs:**
  - Rust tool trait and skill tool constructors change (see BREAKING).
  - TOML gains `skills.mcp`.
  - The model-facing `skill` tool gains MCP entries and an optional `file` argument.
  - Tools-changed events fire when a server's skills change.
- **Docs:** `docs/docs/agent/configuration.md`, `docs/docs/mcp.md`, `docs/docs/agent/dotagents.md`, `docs/docs/agent/dotagents_support_matrix.md`.
- **Out of scope:**
  - `resources/directory/read`.
  - Applying a skill's `allowed-tools`.
  - Remote (URL) skill sources.
  - Mobile FFI and Python binding surfaces.
  - Exposing MCP skills in the `.agents` manifest preview, since they require live connections.
  - Hiding MCP *tools* for a `server.*` denylist entry (pre-existing gap; the new helper enables a follow-up).
  - Forwarding approval prompts from `.agents` delegation targets to the parent's user.
