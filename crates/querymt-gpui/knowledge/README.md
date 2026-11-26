---
id: gpui-repository-map
title: querymt-gpui repository map
description: What the app is, how its layers stack (transport, RPC, store, open sessions, UI, quick panel) and which note explains each.
tags: architecture, map, overview, layers, modules
knowledge_paths: ["src/main.rs", "src/ui/mod.rs"]
knowledge_sources: ["src/main.rs"]
---

# querymt-gpui repository map

A GPUI desktop client for QueryMT agents (`qmtcode`), speaking ACP (JSON-RPC 2.0) to them.
One binary crate, no library target, two build targets from the same UI code: a native
window (macOS, Windows, X11, Wayland) and a wasm32 web app rendered into a `<canvas>`
(`src/main.rs`: `main` for native, `web::start` for wasm, both call `run`).

## Layers

Roughly bottom-up, but not a strict stack. The store owns the connections and creates,
routes inputs to and rebinds the open sessions; an open session never touches the store
entity: it talks to its agent through the RPC client it was handed and reports back with
events (see [sessions.md](sessions.md), "Calls go one way"). The UI and the quick panel
read the store and hold open sessions.

1. **Transport** (`src/transport.rs`): byte pipes to an agent. WebSocket on a native
   background thread (rustls), a local `qmtcode --acp` process over stdio, or the
   browser `WebSocket` on the web. See [agent-connection.md](agent-connection.md).
2. **RPC** (`src/rpc.rs`): minimal JSON-RPC 2.0 client; `src/rpc_log.rs` records traffic
   for the debug events viewer while Developer mode is on.
3. **Store** (`src/store.rs`, `src/store/*.rs`): `AgentsStore`, the one GPUI entity that
   owns agent configs, preferences, connections (`AgentRuntime` per agent), the session
   catalog, pending permission/elicitation requests and the weak index of open sessions.
   See [agent-connection.md](agent-connection.md).
4. **Open sessions** (`src/session.rs`, `src/session/*.rs`): one `OpenSession` entity per
   open conversation, wrapping a `SessionView` read model projected from
   `querymt_agent_session_core::SessionCore`. See [sessions.md](sessions.md).
5. **UI** (`src/ui/*.rs`): `QmtApp` root view with pages, and `SessionPane`, the one
   session surface reused by the main window and the quick panel. See [ui.md](ui.md).
6. **Quick panel** (`src/quick/`, native only): floating HUD chat summoned by a global
   shortcut, tray icon, single-instance socket. See [quick-panel.md](quick-panel.md).

Build, test and CI conventions are in [development.md](development.md).

## Top-level files

| File | Purpose |
|------|---------|
| `../src/main.rs` | Native and wasm entry points, font and icon assets, shared `run`. |
| `../src/transport.rs` | WebSocket, stdio-process and browser transports, one `Connection` shape. |
| `../src/rpc.rs` | JSON-RPC 2.0 client: requests, responses, inbound requests/notifications. |
| `../src/rpc_log.rs` | Per-agent JSON-RPC recorder for the debug events viewer (Developer mode). |
| `../src/store.rs` | `AgentsStore`: configs, prefs, connections, catalog, requests, open-session hub. |
| `../src/session.rs` | The `SessionView` struct, transcript display types (`Item`, `ToolCall`, …) and the session module root. |
| `../src/search.rs` | One fuzzy matcher (frizbee) behind every search box. |
| `../src/theme.rs` | QueryMT design tokens applied to gpui-component, plus the `Palette` global. |
| `../src/highlight.rs` | Standalone syntect highlighting for tool output and Markdown fences. |
| `../src/usage.rs` | Token, cost and active-time model for the session usage popover. |
| `../src/telemetry.rs` | Opt-in OpenTelemetry export (only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set). |
| `../src/profile_templates.rs` | Curated agent profiles copied into `~/.qmt/profiles` on request. |
| `../src/util.rs` | Clock, timestamps, labels. |
| `../build.rs` | Downloads, verifies and caches the pinned IBM Plex fonts for embedding. |

## Where decisions are written down

Module docs cite design decisions as "design Dn". The number refers to one OpenSpec
change's `design.md`, not a global list, and every change numbers from D1, so the same
number means different things in different changes. Some files cite more than one change.
Which change a citation means, by topic:

- **`use-agent-session-core`** (`openspec/changes/use-agent-session-core/design.md`):
  projection D1/D2, overlay D3, input mapping D4, batching and background hydration D6,
  turn helpers D7, differential (golden) tests D8, context indicator D10. Cited from
  `src/session/projection.rs`, `overlay.rs`, `view.rs` (except D9), `golden.rs`,
  `view_tests.rs`, and from `src/session/open.rs` for **D6** (flush and hydration).
- **`add-quick-session-panel`** (`openspec/changes/add-quick-session-panel/design.md`):
  open-session entity D1, one-way calls D2, `SessionKey` newtypes D3, one prompt path D4,
  launch D5, `SessionPane` densities D6, quick panel D7–D18. Cited from
  `src/session/open.rs` (D2–D4), `src/store.rs` (D1, D18), `src/store/launch.rs`,
  `src/ui/session_pane.rs`, `src/ui/session_view.rs` (D6, D15), `src/ui/model_picker.rs`,
  `src/ui/mod.rs`, `src/theme.rs`, `src/ui/workspace_select.rs`, `src/ui/slash.rs` (D17)
  and everything in `src/quick/`.
- **Slash commands and picker search** (archived,
  `openspec/changes/archive/2026-11-24-add-slash-commands-and-picker-search/design.md`):
  early commands D3, commands from the projection D4, palette entries D5, composer command
  list D6, model picker cursor D7, frizbee matcher D8, background load on the view's own
  core D9. Cited from `src/store/early_commands.rs`, `src/store.rs` and
  `src/store/open_tests.rs` (D3), `src/ui/slash.rs` (D4, D6), `src/ui/keys.rs` (D5) and
  `src/session/view.rs` (**D9**).

Other in-flight changes have their own D1…: `quick-panel-status-and-keys`,
`quick-picker-progress`, `slash-command-actions`, `slash-subcommands`. When a citation
does not fit the decision you find, check the change whose topic matches. New citations
should name the change when it is not the file's usual one ("design D6 of
`use-agent-session-core`").

Accepted behaviour lives in `openspec/specs/*/spec.md`; in-flight changes in
`openspec/changes/*/` (proposal, design, tasks, delta specs).

## Desktop parity

Many modules mirror the Svelte desktop app (`querymt-desktop`) and name the desktop
component or store they port in their module doc (for example `src/store/fork.rs` ports
`forkActiveSessionAt`). When behaviour is unclear, the desktop original is the reference.
