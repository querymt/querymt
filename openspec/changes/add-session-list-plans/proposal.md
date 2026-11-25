# Proposal

## Why

A client can show a session's todo list only after it loads that session, because the list reaches clients only as `plan_update`s on an active session. Session pickers and dashboards (the querymt-gpui quick panel first) want to show every listed session's progress and its next steps without loading each transcript. ACP v2 has no plan field on `SessionInfo` and no RFD in flight that adds one. What it does have is a way to extend a listed session: agent data in a namespaced top-level `_meta` key of `session/list` entries, kept current for active sessions with `session_info_update` metadata patches. This change uses that route.

## What Changes

- `session/list` entries for local sessions carry the session's current plans under the top-level `_meta` key `querymt/plans`. The value is an array of ACP `PlanUpdateContent` objects. Today it holds only the todo list: one `items` plan with `planId: "querymt-todos"` and the same entries a `plan_update` for it would carry. Sessions that have no todo list, or whose list is empty, omit the key.
- QueryMT persists each session's current todo plan, so the list reports it across agent restarts. Existing sessions are backfilled from their journals once.
- When a `todowrite` changes a session's plan, QueryMT sends `session_info_update` with a `_meta` patch setting `querymt/plans` (or `null` when the plan is emptied) on connections where the session is active.
- A new extension notification `querymt/session/plansChanged` carries `session_id` and the current `plans`, and is broadcast to every connection. It is listed in the control capabilities' notifications. Clients keep listed sessions current mid-turn with it instead of polling `session/list`.
- **BREAKING** (ACP wire shape, additive): new `_meta` key on `session/list` entries and on `session_info_update`, and a new notification. Clients that ignore unknown `_meta` keys and notifications are unaffected.

## Non-goals

- Plans of sessions listed from mesh peers (`location: "remote"` entries). Peers do not publish plans in their session catalog yet.
- Markdown plans. QueryMT only produces the todo `items` plan. The array shape leaves room for more plans later.
- Moving the existing keys nested under `_meta.querymt` (workspace, pin, lifecycle, model) to top-level keys. Under ACP v2 metadata patches, a `session_info_update` that sends `_meta.querymt` replaces the whole nested object, and `model_changed_update` already does that. This is a known issue for a separate change.
- Making `todoread` return the persisted list after an agent restart. It still reads the in-memory store.
- Reporting `cancelled` todos. They stay out, as in `plan_update`, until ACP plan entries in the protocol crate can express that status.
- UI work in querymt-desktop.

## Capabilities

### New Capabilities
- `session-list-plans`: what `session/list` reports about each session's current plans, how that report is kept current (metadata patches and the `plansChanged` notification), and its persistence across restarts.

### Modified Capabilities
- `session-todos`: deleting a session also discards its persisted todo plan.
- `acp-session-updates`: session metadata synchronization covers the `querymt/plans` key.

## Impact

- Packages:
  - `querymt-agent-protocol-types`: the plans meta key constant and a helper that builds the `querymt/plans` value from todo entries.
  - `querymt-agent-core`: `SessionListItem` gains the current todo entries; storage trait method to write and read them.
  - `querymt-agent-sqlite`: migration `0041_session_todo_plans` (new table plus a one-time journal backfill); list queries join it.
  - `querymt-agent-runtime`: list projection to `_meta`, `session_info_update` patch, the `plansChanged` notification and its capability entry, delete cleanup.
  - `querymt-agent`: tests through the facade.
  - Features touched: `api` (session list API), `remote` (to keep the remote list path untouched and tested).
- Session DB schema: new table `session_todo_plans`, forward-only migration with backfill.
- Public Rust API: new field on `SessionListItem`; new `#[typeshare]` notification params type `SessionPlansChangedNotification`.
- ACP wire shapes: as listed above. CHANGELOG entry under Unreleased.
- Follow-ups:
  - querymt-gpui: the `quick-picker-progress` change consumes this.
  - querymt-desktop: may show progress in its session list; not required.
