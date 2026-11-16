# Proposal

## Why

Loading a remote session can take about 12 seconds, even on a fast LAN. Tracing session `01a1cf38-…` shows the history window arriving in 17 ms. Almost all of the rest of the load waits on one request to the owner for the session's structured user prompts.

To answer that request, the owner reads and decodes the session's whole message history (every tool result, tool call, snapshot and reasoning part), then keeps the few prompt parts. In the measured session that is 8 prompt parts (about 1 KB) out of about 2,700 parts (about 2.9 MB). The owner's session actor handles nothing else meanwhile. Local loads build their prompt list the same way.

The session database makes this worse on slow storage:
- Each session's parts are spread across a large `message_parts` table: 1.26 GB in a 3.6 GB local `sessions.db`, of which 805 MB is tool results and 32 MB is prompts.
- No index orders a session's messages, so every read sorts them.
- No index finds prompt parts without reading the other parts' rows.
- The connection uses SQLite's default 8 MB page cache and no memory mapping, so a cold read goes to disk page by page.

## What Changes

- **Prompt records come from a targeted read.** Only the prompt and steering content of the requested messages is read. A prompt request no longer decodes tool output or other message content, so its cost no longer grows with the session's tool output.
  - Every producer of prompt records uses this read: the owner's reply to a peer's prompt request (used by remote loads and remote older-history pages), the local full and windowed load snapshots, and history replay as session updates for clients without snapshot support. For a remote session, that replay asks the owner for prompt records instead of transferring its full message history.
- **Same results as before.** The returned records (message id, position among the session's messages, timestamp and content blocks), their order, and which prompts a full load includes do not change.
  - One effect is new: a message part that cannot be decoded outside a prompt (for example a corrupt tool result) no longer makes the prompt request, the session load or the history replay fail.
- **Windowed loads select prompts by the window's events.** A windowed local load includes exactly the prompts whose prompt events are in the window. Today it selects by timestamp, which also includes older prompts stored in the same second as the window's first event.
- **Message history order is explicit.** Messages created within the same second are ordered by insertion, in both the full message history and prompt positions, so the two always agree. In the local database 93,084 groups of messages share a second, so this is common. Reading the full message history no longer sorts all of a session's parts in a temporary table.
- **Indexes for session message reads (migration `0037_session_message_read_indexes`).**
  - `messages(session_id, created_at)` gives a session's messages in history order without a sort. It replaces the `messages(session_id)` index, which it makes redundant.
  - A partial index on `message_parts(message_id, sort_order)` restricted to prompt and steering parts lets a prompt read touch only prompt rows.
  - Building both on the 3.6 GB local database took 0.22 s and added about 4 MB.
- **Larger read cache for the session database.** File-backed session database connections use a 64 MiB page cache (`cache_size = -65536`) and memory-map up to 256 MiB of the file (`mmap_size`). Today they use the 8 MB default without memory mapping.
- **Telemetry.** Reading full message history and reading prompt records each get a tracing span with the session id and the number of rows and bytes read. The next slow load then shows where its time goes.

No wire, protocol or configuration changes. The database change adds and replaces indexes only; no stored data changes shape.

## Non-goals

- **Viewer-side cache of prompt records** (keyed by message id, cleared on history rewrite). This would remove the prompt round trip on repeat loads. Left for a follow-up once the fast read is measured.
- **Not waiting for prompt records during a load** (sending the snapshot first and attaching prompts afterwards), and shortening the 60 s wait on the owner.
- **Delta loads** that skip a snapshot the client already holds. With this change, everything a remote load does outside the prompt request takes about 270 ms, so this is not worth doing yet.
- **Caching the whole-session summary pass**, which re-reads the local journal copy on every windowed load.
- **Avoiding the second JSON serialization** of the snapshot that load telemetry does to measure its size.
- **Moving long session-store reads off the shared database connection** (connection pool or a separate read connection).
- **Restructuring stored message content**, such as moving tool-result content to its own table or compacting the event journal (1.14 GB locally). Re-measure with the new spans first.
- **Making the cache sizes configurable.** They are fixed constants; a setting can be added if a deployment needs one.

## Capabilities

### New Capabilities
- `session-prompt-records`: the structured user prompt records served with session loads, older-history pages and peer prompt requests, and the session database reads behind them. Covers:
  - what the records contain, and how they are selected and ordered;
  - the rule that producing them reads only prompt content;
  - the indexes and read cache those reads use.

### Modified Capabilities
None.
- The related requirements live in the main specs `session-history-loading` and `remote-session-sync`, which came from the archived `speed-up-session-loading` change. This change does not alter them.
- `session-storage-reliability` is also unchanged. The read cache requirement lives in this capability because it exists to serve these reads.

## Impact

- **Packages:**
  - `querymt-agent-core` (`crates/agent-core`):
    - `UserPromptRecord`, `user_prompt_records` and `filter_prompt_records` move here from the runtime, into a new module `session::prompt_records`.
    - `SessionStore` gains `get_user_prompts` with a default implementation.
  - `querymt-agent-sqlite` (`crates/agent-sqlite`):
    - the targeted `get_user_prompts` query;
    - `get_history` ordering;
    - the two spans;
    - migration `0037` and the matching `init_schema` change;
    - the connection cache settings;
    - a `tracing-subscriber` dev-dependency for the span tests.
  - `querymt-agent-runtime` (`crates/agent-runtime`), default features and `remote`:
    - `session/load_snapshot.rs` re-exports the moved items, so existing paths keep working.
    - The owner's prompt-request handler builds without `remote`.
    - ACP history replay for clients without snapshot support (`acp/stdio.rs`) switches from full history to prompt records.
    - The remote load and older-history callers are `remote`-only and unchanged.
  - `querymt-agent` (`crates/agent`): tests only. The mesh tests for the owner handler and the existing ACP replay tests live in `src/runtime_tests/`.
  - Downstream (compile check only): `querymt-agent-py` and `querymt-mobile-ffi`.
- **Public Rust API (additive, not breaking):**
  - `SessionStore` (`querymt_agent_core::session::store::SessionStore`, also `querymt_agent::session::store::SessionStore`) gains `get_user_prompts(session_id, message_ids)`. It has a default implementation, so existing implementors and `MockSessionStore` keep compiling.
  - New module `querymt_agent_core::session::prompt_records`. The existing path `querymt_agent::session::load_snapshot::UserPromptRecord` keeps working through the re-export.
  - typeshare: `UserPromptRecord` is now emitted from `querymt-agent-core`. The generated TypeScript type is unchanged; only its position in `types.ts` may move.
- **Database:**
  - Migration `0037_session_message_read_indexes` runs automatically on start. It creates `idx_messages_session_order` and `idx_message_parts_prompt`, and drops `idx_messages_session`.
  - No table or data changes.
- **Resources:** each file-backed session database connection may use up to 64 MiB of page cache, filled only as pages are read, and maps up to 256 MiB of the database file.
- **Behaviour:**
  - A corrupt non-prompt message part no longer fails a session load, a history replay or a peer's prompt request.
  - Same-second messages are ordered by insertion (previously unspecified).
  - A windowed load no longer includes prompt records of older prompts that share a second with the window's first event.
  - CHANGELOG, under Unreleased: these three fixes get one entry under `### Fixed`. The migration and the cache settings get one entry under `### Behavior changes`.
- **Unchanged:** ACP and mesh wire shapes, typeshare TypeScript output, config, hooks, middleware, and the Extism plugin contract. No querymt-desktop follow-up is needed.
- **Rollout:**
  - The remote speed-up only applies once the session's owner runs this version, because the owner answers the prompt request. Mixed versions keep working, because nothing on the wire changes.
  - Older binaries can open a migrated database: they skip the migration record they don't know, and their queries use the new index on `messages`.
