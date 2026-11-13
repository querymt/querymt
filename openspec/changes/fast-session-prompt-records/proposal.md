# Proposal

## Why

Loading a remote session can take about 12 seconds, even on a fast LAN. Tracing session `01a1cf38-…` shows the history window arriving in 17 ms. Almost all of the rest of the load waits on one request to the owner for the session's structured user prompts.

To answer that request, the owner reads and decodes the session's whole message history (every tool result, tool call, snapshot and reasoning part), then keeps the few prompt parts. In the measured session that is 8 prompt parts (about 1 KB) out of about 2,700 parts (about 2.9 MB). The owner's session actor handles nothing else meanwhile. Local loads build their prompt list the same way.

## What Changes

- **Prompt records come from a targeted read.** Only the prompt and steering content of the requested messages is read. A prompt request no longer decodes tool output or other message content, so its cost no longer grows with the session's tool output.
  - Every producer of prompt records uses this read: the owner's reply to a peer's prompt request (used by remote loads and remote older-history pages), the local full and windowed load snapshots, and history replay as session updates for clients without snapshot support. For a remote session, that replay asks the owner for prompt records instead of transferring its full message history.
- **Same results as before.** The returned records (message id, position among the session's messages, timestamp and content blocks), their order, and which prompts a full load includes do not change.
  - One effect is new: a message part that cannot be decoded outside a prompt (for example a corrupt tool result) no longer makes the prompt request, the session load or the history replay fail.
- **Windowed loads select prompts by the window's events.** A windowed local load includes exactly the prompts whose prompt events are in the window. Today it selects by timestamp, which also includes older prompts stored in the same second as the window's first event.
- **Message history order is explicit.** Messages created within the same second are ordered by insertion, in both the full message history and prompt positions, so the two always agree. Reading the full message history no longer sorts all of a session's parts in a temporary table.
- **Telemetry.** Reading full message history and reading prompt records each get a tracing span with the session id and the number of rows and bytes read. The next slow load then shows where its time goes.

No wire, protocol, configuration or database schema changes.

## Non-goals

- **Viewer-side cache of prompt records** (keyed by message id, cleared on history rewrite). This would remove the prompt round trip on repeat loads. Left for a follow-up once the fast read is measured.
- **Not waiting for prompt records during a load** (sending the snapshot first and attaching prompts afterwards), and shortening the 60 s wait on the owner.
- **Delta loads** that skip a snapshot the client already holds. With this change, everything a remote load does outside the prompt request takes about 270 ms, so this is not worth doing yet.
- **Caching the whole-session summary pass**, which re-reads the local journal copy on every windowed load.
- **Avoiding the second JSON serialization** of the snapshot that load telemetry does to measure its size.
- **Moving long session-store reads off the shared database connection.**

## Capabilities

### New Capabilities
- `session-prompt-records`: the structured user prompt records served with session loads, older-history pages and peer prompt requests. Covers what they contain, how they are selected and ordered, and the rule that producing them reads only prompt content.

### Modified Capabilities
None. The related requirements live in `session-history-loading` and `remote-session-sync`, which the unarchived `speed-up-session-loading` change introduces. This change does not alter those requirements.

## Impact

- **Packages:**
  - `querymt-agent` (`crates/agent`), default features and `remote`. The owner's prompt-request handler builds without `remote`. The remote load and older-history callers are `remote`-only and unchanged. ACP history replay for clients without snapshot support (`acp/stdio.rs`) switches from full history to prompt records.
  - Downstream (compile check only): `querymt-agent-py` and `querymt-mobile-ffi`.
- **Public Rust API (additive, not breaking):**
  - The `querymt_agent::session::store::SessionStore` trait gains `get_user_prompts(session_id, message_ids)`. It has a default implementation, so existing implementors and mocks keep compiling.
  - The SQLite store overrides it with the targeted query.
- **Behaviour:**
  - A corrupt non-prompt message part no longer fails a session load, a history replay or a peer's prompt request.
  - Same-second messages are ordered by insertion (previously unspecified).
  - A windowed load no longer includes prompt records of older prompts that share a second with the window's first event.
  - These three fixes get one entry under Unreleased `### Fixed` in `CHANGELOG.md`.
- **Unchanged:** ACP and mesh wire shapes, typeshare types, session DB schema (no migration), config, hooks, middleware, and the Extism plugin contract. No querymt-desktop follow-up is needed.
- **Rollout:** the remote speed-up only applies once the session's owner runs this version, because the owner answers the prompt request. Mixed versions keep working, because nothing on the wire changes.
