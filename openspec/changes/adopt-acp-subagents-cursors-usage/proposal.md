# Proposal

## Why

QueryMT now builds against `agent-client-protocol` 5.4, which exposes standard ACP v1 shapes for three things QueryMT already tracks internally but reports through private channels or not at all: delegated child sessions (`unstable_subagents`), per-turn token usage on the prompt response (`unstable_end_turn_token_usage`), and addressable positions in a session timeline that can be forked from (`unstable_session_cursors`). Clients currently have to understand the QueryMT-only `querymt/session/delegationUpdate` notification and the `_meta.querymt.message_id` fork hint. Adopting the standard shapes lets generic ACP clients see delegations, usage and fork points, and lets QueryMT delete its private delegation notification.

## What Changes

- **Subagents replace the private delegation notification.** When a delegation forks a child session, QueryMT announces it on the parent session with a `subagent_update` (child session id, title = target agent, description = objective, running/idle state, cancel capability). It also attaches a `session` reference to the delegate tool call. Verification and finalization stages are reported as `running` state with `_meta.querymt.stage`. Model, provider node and timestamps go in `_meta.querymt`. These updates are sent only to clients that advertise `subagents`.
- **BREAKING (ACP wire), staged:** the `querymt/session/delegationUpdate` extension notification is removed in the release that switches querymt-desktop; until then it is sent alongside the subagent updates. Clients that do not advertise `subagents` no longer receive delegation metadata. They still receive the delegate tool call and the delegated session's events.
- **BREAKING (ACP wire / typeshare), staged:** the `session/load` snapshot gains `subagentUpdates` (the latest `subagent_update` per child) first; `delegationUpdates` is removed together with the notification.
- **Turn usage on the prompt response:** `session/prompt` responses carry `usage` (total, input, output, thought, cache read, cache write tokens) for the execution cycle that answered the prompt, when at least one provider attempt reported usage. Otherwise `usage` is omitted.
- **Session cursors:** for clients that advertise `session.cursors`, QueryMT emits `cursor_update` anchored after each user message, agent message, agent thought, tool call and compaction, both live and on history replay. Cursors after user and agent messages advertise `capabilities.fork`.
- **Fork at a cursor:** `session/fork` accepts `cursorId`. A forkable cursor forks at its message boundary. Any other cursor id is rejected with `-32602` and never falls back to forking the whole session. `_meta.querymt.message_id` keeps working.
- **qmacp** advertises `subagents` and `session.cursors`. It reports turn usage in `prompt` results, surfaces subagent updates and session references in event output and load snapshots, and gains `fork --cursor`.
- **querymt-desktop** must switch from `querymt/session/delegationUpdate` / `delegationUpdates` to `subagent_update` / `subagentUpdates`. The embedded UI pin is bumped in the same release.

## Non-goals

- `unstable_llm_providers` (`providers/list|set|disable`): QueryMT providers cannot honour client-supplied headers or switch wire protocol, so the capability is not advertised.
- Session-directed messages (`session_message`, `session_message_chunk`) and client-initiated mutations of child sessions beyond cancel.
- Other `unstable_*` features compiled in by the `unstable` bundle (prompt suggestions, session status/title/archive/options, speed category, auth status, MCP over ACP).
- ACP protocol v2.
- Changing how delegated session events are delivered to the parent's client. Delivery already satisfies the subagent model.

## Capabilities

### New Capabilities
- `session-cursors`: cursor announcement for timeline positions and forking a session at an announced cursor.
- `qmacp-client`: what the `qmacp` ACP client advertises and how it reports the new updates.

### Modified Capabilities
- `acp-session-updates`: adds turn usage on prompt responses and extends per-connection capability isolation to cursor and subagent updates.
- `agent-delegation`: delegated child sessions become visible through subagent updates and tool-call session references, replacing the QueryMT delegation notification.

## Impact

- **Packages:** `querymt-agent-protocol-types` (delegation update and load snapshot types, shared cursor helper in `replay`), `querymt-agent-core` (re-exports), `querymt-agent-services` (delegation projection), `querymt-agent-runtime` (ACP translator, replay, load snapshot, fork, prompt response, capability parsing), `querymt-agent` (WebSocket ACP tests, facade), `qmacp`, `querymt-mobile-ffi` (snapshot passthrough tests), `skills/qmacp` docs.
- **Feature flags:** relies on `agent-client-protocol` features `unstable` (includes `unstable_subagents`, `unstable_end_turn_token_usage`, `unstable_session_cursors`) and `schemars`. No new QueryMT Cargo features. Runtime code paths sit under the default runtime features and `remote` for mesh-hosted delegations.
- **ACP wire shapes:**
  - New `subagent_update`, `cursor_update`, `ToolCallContent::session`, `PromptResponse.usage` and `ForkSessionRequest.cursorId` handling.
  - **BREAKING:** removal of `querymt/session/delegationUpdate`.
  - **BREAKING:** `delegationUpdates` → `subagentUpdates` in load snapshots.
- **Public Rust API / typeshare:** `SessionLoadSnapshot` (now in `querymt-agent-protocol-types`) gains `subagent_updates`; **BREAKING** (with the desktop switch): `delegation_updates` is removed. `DelegationUpdateNotification` stops being a wire type.
- **Config, hooks, middleware, Extism plugin contract, session DB schema, Python bindings:** unchanged.
- **Coordination:** `add-agent-session-core` already reads only `subagent_update`, tool-call `session` references and `subagentUpdates`; its task 4.5b uses this change's cursor helper (3.1). querymt-gpui's `use-agent-session-core` needs 1.1, 4.1, 4.2 and 4.5 on the agent it talks to.
- **querymt-desktop follow-up (required):** consume `subagent_update` and `subagentUpdates` instead of the removed delegation notification, advertise `subagents` (and optionally `session.cursors`), then bump `crates/agent/embedded-ui-revision` and the flake pin.
