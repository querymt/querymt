---
id: gpui-sessions
title: Open sessions, the session core and the transcript projection
description: How a conversation is held open (OpenSession, SessionKey, the store's weak hub), how agent inputs reach SessionCore and are projected into transcript Items, batching, background hydration, the single prompt path and steer/queue delivery.
tags: session, open session, transcript, projection, reducer, session core, overlay, prompt, steer, queue, flush, hydration, lifetime, undo, fork
knowledge_paths: ["crates/querymt-gpui/src/session.rs", "crates/querymt-gpui/src/session/*.rs", "crates/querymt-gpui/src/ui/session_pane.rs"]
knowledge_sources: ["crates/querymt-gpui/src/session/open.rs", "crates/querymt-gpui/src/session/view.rs", "crates/querymt-gpui/src/session/projection.rs", "crates/querymt-gpui/src/session/overlay.rs", "crates/querymt-gpui/src/store.rs#AgentsStore::open"]
---

# Open sessions, the session core and the transcript projection

Design decisions referenced here: `openspec/changes/use-agent-session-core/design.md`
(reducer, projection, overlay, batching: D1–D10) and
`openspec/changes/add-quick-session-panel/design.md` (open-session entities, events, panes:
D1–D6). The two overlap in numbers: `src/session/open.rs` cites D2–D4 of the quick-panel
change but D6 (flush, hydration) of `use-agent-session-core`, and `src/session/view.rs`
cites D9 of the archived slash-commands change. See the map in [README.md](README.md).

## Ownership: one entity per open session

- `SessionKey { agent: AgentId, session: SessionId }` identifies a session; `SessionRef { key,
  cwd, title }` is what a surface asks to open. The newtypes exist only at the open-session
  boundary; the rest of the store keeps `&str` (`src/session/open.rs`).
- `OpenSession` is a GPUI entity holding `key`, `view: SessionView` and a `Link` (the
  agent's `Rc<RpcClient>`, `connected`, `generation`, `SessionCaps`).
- `AgentsStore::open(SessionRef)` returns the live entity from the store's hub
  (`HashMap<SessionKey, WeakEntity<OpenSession>>`) or creates one and starts `session/load`.
  A session whose last load failed is reloaded when opened again.
- **Lifetime is the strong handles.** Surfaces (`SessionPane`, the quick panel's
  `QuickState`) hold `Entity<OpenSession>`; the store holds only weak ones. When the last
  surface drops its handle the session closes; the next input routed to it finds a dead
  weak entry and prunes it. There is no manual reference counting and no close call to the
  agent.

## Calls go one way

GPUI panics on re-entrant entity updates, so:

- The store may call into an `OpenSession` (routing inputs, `rebind(link)` on
  connect/disconnect, `reload`).
- An `OpenSession` never reads or updates `AgentsStore`. It talks to the agent through its
  `Link`, and anything the store must do afterwards leaves as an `OpenSessionEvent`
  (`TurnFinished`, `Loaded`, `Cancelled`), delivered after the update ends and handled in
  `AgentsStore::on_session_event`.

## From agent inputs to rows

- `querymt_agent_session_core::SessionCore` (`crates/agent-session-core`, a path
  dependency in the same workspace) is the **only writer** of conversation state. `SessionView` (struct in
  `src/session.rs`, methods in `src/session/view.rs`) wraps it with a projection
  (`src/session/projection.rs`) and an `Overlay` (`src/session/overlay.rs`).
- Agent notifications become `SessionInput`s (`queue_session_update`, `queue_state_update`,
  `queue_runtime`, `queue_input_state`) and are queued. `OpenSession::schedule_flush`
  applies the queue once after `SESSION_FLUSH_MS` (8 ms): a burst of streamed chunks is one
  batch, one `ChangeReport` and one `cx.notify()` on that session only. Prompt, history and
  load actions call `feed`, which flushes the queue first so order is kept.
- The projection maps each turn to a contiguous slice of `items: Vec<Item>` and rebuilds
  only the turns the change report names, so unchanged rows keep their `key`, `rev` and
  `Arc<ToolCall>` pointers (the row cache in `src/ui/session_view.rs` compares them).
  `Item.key` is the core entry id; consecutive reasoning and tool entries group into one
  `ItemKind::Work` keyed by its first entry.
- The overlay holds client-only state the core leaves out: notices anchored to turn ids,
  load/history errors, modes, access, tool content fetches and steer/queue inputs the agent
  has not confirmed yet.
- **Background hydration.** A `session/load` response over `HYDRATE_BYTES` (256 KiB) is
  decoded on `cx.background_executor()` (native only; wasm hydrates inline). While it runs
  `flush` leaves the queue alone, so live inputs apply after the load, in order.

## Sending

- `Delivery::{Prompt, Steer, Queue}` maps to `session/prompt`, `_querymt/session/steer`,
  `_querymt/session/queue` in one exhaustive `match`.
- `OpenSession::start_prompt` is the **one** place a turn starts (launch's first prompt
  and later sends): optimistic user row with a `client_prompt_id` (`gpui-<ms>-<n>`),
  `prompting = true`, `session/prompt` with `_meta.querymt.client_prompt_id`, then
  `finish_prompt` feeds `PromptCompleted`/`PromptFailed`, adds stop-reason notices,
  refreshes the undo stack and emits `TurnFinished`.
- Steer/queue inputs get a local id and are re-keyed to the agent's id from the submit
  response; `session/inputState` updates that arrive earlier are buffered.

## Turns, undo and fork

Turn helpers (`turn_range`, `turn_text`, undo targets, fork points, turn diffs) work on the
projection's turn slices, not by scanning for user rows. Undo rolls back a whole run to its
first user message id; fork uses the turn's last agent-identified reply (D7).

## Testing

- `src/session/golden.rs`: ported gpui scenarios compared with `tests/golden/session/`
  (update deliberately with `UPDATE_GOLDEN=1` and review the diff), plus QueryMT's
  `testing::golden_cases()` checked for core errors, empty turns and incremental-vs-rebuilt
  equality.
- `src/session/view_tests.rs`: input wiring and turn tables.
- `src/store/open_tests.rs`: several sessions open at once, routing, sharing, lifetime,
  reconnects (spec `open-sessions`, still a delta spec in
  `openspec/changes/add-quick-session-panel/specs/`, not yet in `openspec/specs/`).

## Files

| File | Purpose |
|------|---------|
| `../src/session.rs` | The `SessionView` struct, display types (`Item`, `ItemKind`, `ToolCall`) and the session module root. |
| `../src/session/open.rs` | `SessionKey`, `SessionRef`, `Delivery`, `Link`, `OpenSession` and its events. |
| `../src/session/view.rs` | `impl SessionView`: queueing, flushing, prompts, inputs, turn tables; `next_client_id`. |
| `../src/session/projection.rs` | Incremental projection of `SessionCore` turns into `Item` rows. |
| `../src/session/overlay.rs` | Client-only session state the reducer leaves out. |

Related: [agent-connection.md](agent-connection.md) for how inputs are routed to a session,
[ui.md](ui.md) for `SessionPane`, which shows one.
