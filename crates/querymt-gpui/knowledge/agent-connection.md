---
id: gpui-agent-connection
title: Agent connections, RPC routing and the store
description: How AgentsStore connects to an agent (transport, initialize, generations, reconnect), routes inbound JSON-RPC, handles permission and elicitation requests, and persists configs and prefs.
tags: store, connection, transport, rpc, websocket, acp, reconnect, generation, notifications, requests, permission, elicitation, prefs, persistence
knowledge_paths: ["crates/querymt-gpui/src/store.rs", "crates/querymt-gpui/src/store/*.rs", "crates/querymt-gpui/src/transport.rs", "crates/querymt-gpui/src/rpc.rs", "crates/querymt-gpui/src/rpc_log.rs"]
knowledge_sources: ["crates/querymt-gpui/src/store.rs#AgentsStore::connect", "crates/querymt-gpui/src/store.rs#AgentsStore::on_incoming", "crates/querymt-gpui/src/store.rs#AgentsStore::fail", "crates/querymt-gpui/src/transport.rs", "crates/querymt-gpui/src/rpc.rs"]
---

# Agent connections, RPC routing and the store

`AgentsStore` (`src/store.rs`) is the app's one data entity. It owns `configs`
(`AgentConfig`: id, name, `Endpoint::{Process, WebSocket}`, `auto_start`, `host_managed`
for the web build's own server, which cannot be edited or deleted), `prefs`
(`Prefs`), one `AgentRuntime` per agent, the pending `requests`, and the weak index of open
sessions. Views observe it and re-render on `cx.notify()`. Feature areas that grew large live
in `src/store/*.rs` as `impl AgentsStore` blocks or helpers (automations, delegates, fork,
launch, worktree, pins, plans, plugins, mesh, setup, sidebar order).

## Connecting

`AgentsStore::connect` is one async task: `transport::connect(target)` →
`RpcClient::new(outgoing)` → a reader task → `initialize` → `load_details` (capabilities,
models, session catalog, profiles, providers, mesh).

- **Transport** (`src/transport.rs`): every backend returns the same `Connection { outgoing:
  Sender<String>, incoming: Receiver<Inbound>, pid }`. `Inbound` is `Message` (a JSON-RPC
  line), `Log` (agent stderr), `Note` (transport notes) or `Closed(reason)`. Closing the
  outgoing sender tears the transport down. Local processes are native only; the browser
  can only open WebSockets.
- **RPC** (`src/rpc.rs`): `RpcClient::request` returns a future for the response;
  `handle(text)` matches responses to pending requests and returns `Incoming::{Request,
  Notification}` for everything else; `fail_all` fails pending requests when the transport
  closes.
- **`initialize` capabilities** matter for what the agent sends: `plan`, `subagents`,
  `session.compaction`, `elicitation.form` and `_meta.querymt.sessionLoadSnapshot.epoch`
  (the transcript is built from the `session/load` snapshot, so the agent skips replaying
  history as `session/update`s). The session reducer is tested against exactly these shapes.

## Generations

Each `connect`/`disconnect` bumps `AgentRuntime.generation` and clears `pending` (the action
tags in flight belong to the old connection). Every async continuation looks the runtime up
through `live(id, generation)` and does nothing when it no longer matches, so a late
response from an old connection can never touch the new one. Closing the old `RpcClient`
fails its pending requests, so those continuations do run, with `Err("Disconnected.")`:
the generation check is what keeps them out. Do the same in any new async store code
(test: `ext_answer_from_an_old_connection_is_ignored`).

## Failure and reconnect

`fail` closes the RPC client, marks `ConnState::Failed`, retires requests, rebinds open
sessions to the dead link and calls `schedule_reconnect`. Only WebSocket agents with
`auto_start` reconnect, backing off 250 ms doubling to 8 s (`reconnect_delay`, like the web
client). After a successful reconnect, `rebind_sessions(id, reload = true)` reloads every
open session, because their live updates were lost with the old connection.

## Inbound routing (`on_incoming`)

- **Requests** from the agent (`session/request_permission`, elicitations) are parsed by
  `parse_request` into a `PendingRequest` card; anything unknown is answered `-32601`.
  `receive_request` dedupes re-deliveries by key: a re-delivered permission supersedes the
  old RPC id, a rejected answer re-opens the card keeping its draft. On disconnect,
  `retire_requests` leaves cards with a stable identity `offline` (waiting for re-delivery)
  and resolves the rest as "No longer pending". Cards render in the Inbox and inline in the
  session (`src/ui/inbox.rs`).
- **Session notifications** are addressed with `SessionKey::from_update(agent, params)` and
  delivered only to a live `OpenSession` (`session/update`, `stateUpdate`, `runtimeChanged`,
  `inputState`), which queues them and schedules its own flush. Updates for a session no
  surface holds change no transcript; `session/update`s arriving before a launched view
  exists are kept as early slash commands (`src/store/early_commands.rs`).
- **List-level notifications** (runtime/lifecycle/update) schedule a debounced
  `session/list` refresh of the first page (1.2 s), so the sidebar phase stays current;
  pins, plans, mesh membership, MCP auth, plugin updates, schedules, delegate models and
  model catalog changes each have their own handler in `src/store/*.rs`.
- QueryMT extension methods are matched tolerant of a leading `_` (`_querymt/...` and
  `querymt/...`, via `trim_start_matches('_')` or `ends_with`); `session/update` and
  `elicitation/complete` are matched exactly.

## Extension calls

`AgentsStore::ext(id, tag, method, params, cx, then)` is the helper for one-off agent
methods: it marks `tag` pending on the runtime (`is_pending`) and sends the request. When the
answer arrives on the same generation it clears the tag, records an error in
`action_error` and runs `then`; after a connect or disconnect it does nothing (the tag was
already cleared, and the same action may be pending again on the new connection).

## Persistence

`Persisted { configs, prefs }` is saved as JSON to `<config_dir>/querymt-gpui/state.json`
natively and to `localStorage["querymt-gpui:state"]` on the web. New `Prefs` fields must use
`#[serde(default)]` so older files still load (tests: `old_prefs_load_with_quick_defaults`).
First-run seeds (`seed_configs`): `--agent host:port` / `QMT_AGENT` adds a WebSocket agent;
`qmtcode` on `PATH` adds "Local qmtcode" (`qmtcode --acp`); on the web, the serving origin's
`/acp/ws` or `?agent=`.

## Files

| File | Purpose |
|------|---------|
| `../src/store/launch.rs` | Where a new session runs: local or mesh peer, checkout or new worktree, peer profiles. |
| `../src/store/early_commands.rs` | Slash commands advertised before a launched session's view exists. |
| `../src/store/delegates.rs` | Delegate model routing of a session's profile. |
| `../src/store/fork.rs` | `session/fork` from a reply or the last turn. |
| `../src/store/worktree.rs` | Worktree session creation and handoff. |
| `../src/store/automations.rs` | ACP schedule catalog and mutations. |
| `../src/store/sidebar.rs` | Stable, user-ordered open/pinned session lists and attention markers. |
| `../src/store/plans.rs` | Plans (todo lists) of listed sessions without opening them. |
| `../src/store/pins.rs` | Live pin changes from other clients. |
| `../src/store/mesh_live.rs` | Mesh join/peer-expired handling. |
| `../src/store/mcp_notice.rs` | MCP servers a session had to skip (missing OAuth). |
| `../src/store/plugins.rs` | Provider plugin update progress. |
| `../src/store/setup.rs` | First-run readiness and onboarding state. |
| `../src/store/agent_log.rs` | Per-agent log: app notes plus parsed agent stderr. |
| `../src/store/devtools.rs` | Developer mode toggle and wire recording. |

Related: [sessions.md](sessions.md) for what an open session does with the inputs routed to it.
