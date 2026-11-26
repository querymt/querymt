# Proposal

## Why

The quick panel's session picker shows only a title, the agent and folder, and a status chip. It does not say how far a session has got or what it is doing next. The agent's todo list (`todowrite`) has that information, but the app sees it only for sessions it has loaded. querymt's `add-session-list-plans` change adds each session's plans to `session/list`, plus a notification when they change. That makes it possible to show progress for every listed session and to try a richer picker layout.

## What Changes

- **Plans of listed sessions.** The app reads `_meta["querymt/plans"]` from `session/list` entries, an array of ACP plan contents.
  - It keeps a listed session's plans current from the `querymt/session/plansChanged` notification, without re-listing.
  - A session open in the app uses its live plan instead.
  - Agents that do not send the key show no progress; nothing else changes.
- **Progress in the list layout.** Each picker row gets a background fill as wide as the share of completed todos, and a `done/total` count. Rows without todos are unchanged.
- **Card grid layout.** A second picker layout shows the sessions as cards in two columns. Each card has:
  - the title and status;
  - agent · workspace · last updated;
  - a progress bar with `done/total`;
  - the in-progress todo and the next pending ones, then "+N more".
- **Layout switch.** A toggle in the picker's search row switches between the list and the grid. The choice is saved in the quick panel settings, and the list is the default.
- The picker keeps its current order (waiting, then running, then newest) and its limit of eight sessions, in both layouts.

## Non-goals

- The horizontal, draggable strip of session cards. It was considered and left out; the grid comes first.
- Progress in the main window's sidebar. The data is available there after this change, but the sidebar is not touched.
- Keyboard navigation in the picker, in either layout. The picker is mouse-only today.
- Markdown plans. querymt does not produce them; plans of another `type` are ignored.
- Persisting plans on the client. The agent persists them (`add-session-list-plans`).

## Capabilities

### New Capabilities
- `session-plans`: what the app knows about each listed session's plans, where that comes from (session list, change notification, live session), and how it is kept current.

### Modified Capabilities
- `quick-panel`: the session picker shows progress, offers the card grid and the layout switch, and sizes itself for the grid. The capability is introduced by the in-flight `add-quick-session-panel` change, so this change adds requirements rather than modifying existing ones. It is listed here because its behaviour changes.

## Impact

- Code:
  - `src/store.rs`: `SessionSummary` gains plans parsed in `from_list_entry`; handling of `querymt/session/plansChanged`; a plan lookup that prefers the live session; `QuickPrefs.picker_layout`.
  - `src/session.rs`: a `PlanDigest` (done, total, entries) next to `PlanEntry`, and the priority on `PlanEntry` if the cards use it.
  - `src/quick/panel.rs`: `PickerRow.plan`, the fill in the list rows, the grid, the toggle, and a layout-aware height floor.
  - `src/quick/tests.rs`: tests listed in tasks.
- Dependency: needs a querymt agent with `add-session-list-plans` for full coverage. The pinned `querymt-agent-session-core` revision does not need to change, because the parsing is client-side JSON.
- Settings file: a new optional `quick.picker_layout` key. Old settings files load with the default.
- Depends on `add-quick-session-panel` (the panel and picker it changes). That change should be archived first, or this one archived after it.
