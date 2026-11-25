# Design

## Context

- A session's todo list exists in two forms today:
  - the in-memory `TodoStore` (per agent config, lost on restart), which `todoread` reads;
  - the journal: every `todowrite` is a `ToolCallStart` event whose arguments hold the full list.
- The ACP plan is projected from the journal form: `todo_entries_from_arguments` turns the arguments into `PlanEntry`s. The live translator (`acp/shared.rs`, `translate_todo_updates`), the replay path and the client reducer (`agent-session-core/src/project.rs`) all project from the same `ToolCallStart`, so a plan is reported when the call starts, before the tool validates it.
- `session/list` for local sessions is built from `SessionListItem` rows (`agent-sqlite` view store) and projected to ACP in `session_list_item_to_acp_info` (`api/sessions.rs`). Remote entries are built from the peer catalog in the same file.
- Every journal write goes through one function, `insert_journal_event` (`agent-sqlite/.../event_journal.rs`). History can be cut back with `delete_session_events_range`.
- The pins feature shows how agent-wide notifications work: `emit_session_pins_changed_notification` broadcasts an ext notification to every connection, and `control/capabilities.rs` lists it.

ACP v2 findings that shape the wire format:
- `SessionInfo` has no plan field, and no RFD proposes one.
- `session/list` entries carry agent data in `_meta`. `session_info_update` patches it by top-level key: an omitted key is unchanged, `null` removes it, and any other value replaces it whole.
- Plan content is `PlanUpdateContent` (`items` or `markdown`), identified by `planId`.

## Goals / Non-Goals

**Goals:**
- The listed plan is exactly what a client would hold after replaying the session, in every case including rejected and rewound writes.
- Listing stays one query, with no journal scan per session at list time.

**Non-Goals:**
- Making `TodoStore` persistent or changing `todoread` (see proposal).

## Decisions

### D1. Wire shape: top-level `_meta["querymt/plans"]`, holding `PlanUpdateContent[]`

The key holds an array of ACP plan content objects, today one `items` plan with id `querymt-todos`:

```json
"_meta": {
  "querymt/plans": [
    { "type": "items", "planId": "querymt-todos",
      "entries": [{ "content": "Add table", "priority": "high", "status": "completed" }] }
  ]
}
```

- **Why a top-level key and not `_meta.querymt.plans`:** v2 patches replace top-level keys whole. A plans patch nested under `querymt` would wipe the workspace, pin and lifecycle values a client stored from the list. A key of its own is the unit of ownership the patch RFD describes. The `/` form follows the docs' `com.example/messageCount` example.
- **Why `PlanUpdateContent` rather than counts or a custom shape:** clients already parse it from `plan_update`. It carries the entries the cards need (the user chose entries over counts), keeps `planId`, and leaves room for more plans and the markdown type without changing the shape.
- **Alternative considered:** a `querymt/session/plans` request per session. It costs one round trip per row and does not fit the list refresh clients already do.

### D2. Persist the projected plan at journal write time

- **Storage:** a new table `session_todo_plans(session_id INTEGER PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE, entries_json TEXT NOT NULL, updated_at TEXT NOT NULL)`. It follows the side-table pattern of `session_lifecycle` and `session_pins`.
- **Writes:** `insert_journal_event` runs the same projection the replay uses on any todowrite `ToolCallStart`, inside the insert's transaction.
  - Non-empty entries upsert the row; empty entries delete it.
  - Arguments that are not a todo list (`None`) leave the row alone.
- **Effect:** listing, live updates and replay share one source and one projection function, so they cannot drift apart. A todowrite the tool later rejects therefore updates the listed plan, just as it already updates the live and replayed plan.
- **Alternative considered:** writing from the `todowrite` tool through `TodoStore`. It would disagree with `plan_update` for rejected calls, and the tool layer has no access to the database.
- **Alternative considered:** computing the plan at list time with a subquery for the last todowrite event per session. It costs a journal scan per listed session on every list call.

### D3. Rewinds recompute the plan

- After `delete_session_events_range` removes events, the session's row is recomputed from the last remaining todowrite `ToolCallStart`, or deleted if none is left.
- The same recompute function performs the migration backfill.
- The prune happens in `cleanup_revert_on_prompt`, when the next prompt starts. There the runtime compares the last todowrite before and after the pruned range. When it differs, the todo list changed without a todowrite, so:
  1. the in-memory `TodoStore` that `todoread` reads is set to the surviving list (it holds the full items, including ids and cancelled ones, so it is rebuilt from the surviving call's arguments);
  2. when the plan clients see changed, a transient `TodoPlanRestored { entries }` event is emitted on the session's stream. `todo_plan_of` reads it like a todowrite, so the per-connection translator and announcer handle it with the same rules (patch, `plan_update` / `plan_removed`, `plansChanged`).
- **Why transient:** the recomputed row and the pruned journal are already authoritative, so the event is only a live hint (as `DelegateModelsChanged`). A durable kind would be persisted in journals that older loaders reject, which makes rewound sessions unreadable there. The cost: a connection that lags loses the hint, which the lag catch-up (D6) replaces with a read of the stored plan.

### D4. Migration `0041_session_todo_plans` with backfill

- The migration creates the table, then for each session runs the D3 recompute, reading the last `ToolCallStart` whose tool name is `todowrite` or `mcp_todowrite` from `event_journal`.
- It is forward-only and appended after `0040`.
- `legacy_baseline_upgrade_matches_fresh_install` must keep passing. The backfilled table is empty for a fresh install and for a legacy database without todo events.

### D5. Projection to ACP

- `SessionListItem` gains `todo_plan: Option<Vec<PlanEntry>>` (serde-skipped when `None`), filled by the list queries through a `LEFT JOIN`.
- `session_list_item_to_acp_info` inserts `querymt/plans` when it is `Some` and non-empty. The value is built by a helper in `agent-protocol-types` next to `todo_plan_update`, so the list and the live update share one constructor.
- Remote peer entries never set it.

### D6. Change announcements

- **Where:** per connection. The stdio and WebSocket transports each run `announce_session_plans` on the agent's event stream next to their other per-connection forwarders; the ACP translator already sees a todowrite `ToolCallStart` on the same stream. One announcer per connection, never one per agent handle: handles share one event stream, so per-handle announcers would announce every change several times.
- **One projection:** `todo_plan_of(kind)` in `replay.rs` decides what plan an event sets. The journal, the translator, the materialized state and the announcer all use it.
- **Dedupe:** each connection compares the projected plans with the last value it announced for the session, kept per session in memory for the life of the connection. If they are equal, it sends nothing. The translator's cache is also seeded from the history a load replays (keeping a value observed live), so a later emptied list removes the plan the client received by replay.
- **Falling behind:** when a connection lags, the durable events come back from the journal, but the ephemeral rewind hint does not. The connection therefore reads the stored plans (`ViewStore::get_session_todo_plans`; the default is unsupported, so a mock or foreign store never reads as "no plans") and sends what differs from what it announced. The announcer first drops its queue with `resubscribe` (the store already reflects it; replaying it would announce older plans after the newer one), then reconciles the sessions it announced before. The stdio and WebSocket lag branches call `reconcile_session_plans` for the sessions they track. If the store cannot answer, the announcer forgets what it announced instead.
- **Alternative considered:** a persisted revision per plan row, so the announcer can ask for every plan changed after a baseline. It also covers sessions never announced on the connection, but needs a column and index, and an emptied plan would stay as a tombstone row instead of being deleted. The gap it closes (one missed announcement for a session first changed during a lag) does not justify that.
- **What it sends when they differ:**
  1. The `session_info_update` patch (`{"querymt/plans": [...]}`, or `null` when emptied), through the normal per-connection session update path, so it reaches only connections where the session is active.
  2. `querymt/session/plansChanged` `{session_id, plans}`, from the connection's announcer. The active connection receives both; each sets the whole value.
- **Why include `plans` in the notification:** clients update the row without a `session/list` round trip, which matters mid-turn when a session is running but not open.
- **Typeshare:** the params type is `#[typeshare] SessionPlansChangedNotification`.
- **Alternative considered:** reusing `runtimeChanged`. It carries runtime state, not plans, and clients would have to re-list.

### D7. Cancelled entries stay out

- The listed entries use `todo_entries_from_arguments` unchanged, so `cancelled` todos are dropped, as in `plan_update`.
- ACP v2's `PlanEntryStatus` now has `cancelled`. When the pinned protocol crate gains it, the projection changes once for every path.

## Risks / Trade-offs

- **[Risk] Write cost:** every journal insert pays a projection check.
  - **Mitigation:** a cheap tool-name match first; the JSON parse runs only for todowrite.
- **[Risk] Backfill time:** large journals make the migration slow.
  - **Mitigation:** one indexed query per session for the last todowrite event (`event_journal` is indexed by session and sequence), and it runs once.
  - **Measured:** the same query for every session of a real database (4.1 GB, 8,087 sessions, 1.65 M journal events, 218 k tool-call starts) takes 0.18 s warm; the plan is `SEARCH ... USING INDEX idx_event_journal_session_seq (session_id=?)`.
- **[Risk] Stale dedupe state:** each connection's in-memory "last announced" map grows with the sessions it sees and dies with the connection, so the first write of a session on a connection is always announced.
  - **Mitigation:** acceptable; the map is bounded by the connection's lifetime.
- **[Trade-off] Rewind is announced late and best effort:** the stored plan changes when the next prompt starts, not when the undo runs, so the announcement comes then. It is a transient event (D3): a connection that was not listening gets the right plan from its next `session/list`, and one that lags catches up from the stored plan (D6).
- **[Trade-off] Lag catch-up covers announced sessions only:** a session that first changed plans during a lag, on a connection that never announced it, is not announced; `session/list` reports it and its next `todowrite` is announced.
- **[Trade-off] Rejected writes:** a rejected todowrite changes the listed plan, which is consistent with the current `plan_update` behaviour.
  - **Follow-up:** if that behaviour is changed (projecting on a successful tool result instead), it changes in the one shared projection.
- **[Risk] Collision with an existing key:** clients that store `_meta` from the list may already keep `querymt` keys.
  - **Mitigation:** `querymt/plans` is a new top-level key and does not collide.

## Migration Plan

- Deploy: the new agent version runs migration `0041`. Clients that do not know the key ignore it.
- Rollback: the table is unused by older binaries; leaving it in place is harmless.

## Open Questions

- Should `todoread` fall back to the persisted plan after a restart? This is deferred: it does not change this change's wire shape or tasks.
