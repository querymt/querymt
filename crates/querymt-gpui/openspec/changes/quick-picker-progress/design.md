# Design

## Context

- **Session list:** `SessionSummary::from_list_entry` (`src/store.rs:333`) builds a summary from each `session/list` entry and its `_meta`. `merge_sessions` (`src/store.rs:3960`) replaces listed summaries on refresh.
- **Refresh triggers:** the store refreshes the first page 1.2 s after `runtimeChanged`, `lifecycle` and `session/update` notifications (`src/store.rs:3868`) and 300 ms after a turn ends.
- **Extension notifications:** they are routed in the same `match` on `method`, for example `querymt/session/pinsChanged` goes to `store/pins.rs`.
- **Open sessions:** they are `Entity<OpenSession>` values indexed in `open_sessions.hub` by `SessionKey`, with `live_session(key)` at `src/store.rs:3019`. Their `view.plan: Vec<PlanEntry>` (`content`, `status` strings) follows live `plan_update`s.
- **Existing plan UI:** the title-bar plan popover (`src/ui/session_view.rs:4890-5000`) already draws `done/total` and per-entry icons (`CircleCheck`, `CircleDashed`, `Circle`) with `success`, `running` and `dim` colours.
- **Picker:** the quick panel builds `PickerRow`s in `picker_rows` (`src/quick/panel.rs:1337`), sorted waiting, then running, then newest, and truncated to `PICKER_ROWS` = 8. It renders them in `render_picker` (`src/quick/panel.rs:862`) as an absolutely placed overlay.
- **Height:** `height_floor` (`src/quick/panel.rs:455`) reserves `PICKER_CHROME + rows * PICKER_ROW`. The panel is 680 px wide, and `fitted_height` caps the height at `min(760, 0.7 * display)`, with floors limited to `0.9 * display`.
- **Data source:** the wire shape comes from querymt `add-session-list-plans`. `_meta["querymt/plans"]` is a `PlanUpdateContent[]`, and `querymt/session/plansChanged` carries `{session_id, plans}`. This client does not handle `session_info_update` today, and does not need to for this change.

## Goals / Non-Goals

**Goals:**
- One place that answers "what is session X's plan" for any view, so the sidebar can use it later without new plumbing.
- Both layouts can be built and compared before the querymt change ships. Sessions open in the app show progress from their live plan either way.

**Non-Goals:**
- A virtualized or paged picker. It stays at eight sessions.

## Decisions

### D1. Plans live on `SessionSummary`, parsed once

- **Data:** `SessionSummary` gains `plans: Vec<ListedPlan>` where `ListedPlan { id: String, entries: Vec<PlanEntry> }`. Only `type == "items"` elements are kept.
- **Parsing:** `from_list_entry` parses them with the same status strings the live projection uses.
- **Effect:** `merge_sessions` replaces them with the rest of the summary on each refresh, so there is no separate cache to invalidate.
- **Alternative considered:** a side map in the store keyed by `SessionKey` (the earlier plan). It needs its own invalidation and was only useful while the list had no plans.

### D2. `plansChanged` patches the summary in place

- **Handling:** a new branch before the refresh check in the notification `match` parses the params with the same parser as D1. It finds the summary by agent id and session id in `rt.sessions` and replaces its `plans`, then calls `cx.notify()`.
- **Unknown sessions:** ignored. The next list refresh will bring them.
- **Effect:** no `session/list` round trip, so progress moves mid-turn for sessions that are not open.

### D3. `PlanDigest` and one lookup that prefers the live plan

- **`PlanDigest`** in `src/session.rs`:
  - fields `done`, `total`, `entries`;
  - `fraction()`;
  - `current()`, the first `in_progress` entry;
  - `upcoming(n)`, the first `n` `pending` entries;
  - `hidden(shown)`.
- **Building it:** from a plan list, it picks `querymt-todos` or the first plan. An empty plan gives `None`.
- **Lookup:** `AgentsStore::plan_digest(&self, summary) -> Option<PlanDigest>`.
  1. If `open_sessions.hub` holds a live entity for the summary's key and its `view.plan` is non-empty, use that.
  2. Otherwise use `summary.plans`.
  - The lookup takes `&self`, so it upgrades the weak entity without pruning.
- **The title-bar popover's count** is computed with `PlanDigest` too, so the two counts cannot differ.
- **Alternative considered:** pushing live plans into `SessionSummary` on each plan update. It would need a new `OpenSessionEvent` and would be overwritten by every list refresh.

### D4. `PickerRow.plan` and the list fill

- **Row data:** `picker_rows` fills `plan: Option<PlanDigest>` through D3. Sorting and truncation are unchanged.
- **Row shape:** the row becomes `relative().overflow_hidden()`. Its first child is an absolute fill, `left(0) top(0) bottom(0)` with width `relative(fraction)`.
- **Fill colour:**
  - `success` at 100%;
  - `accent` while running;
  - `dim` otherwise;
  - at 14% alpha in each case.
- **Count:** `done/total` in `kit::MONO`, `text_xs`, `p.muted`, before the tone chip.
- **Readability:** text sits over the fill at 14% alpha. A unit test checks the contrast of `p.text` over the fill blended on `p.surface` for both HUD palettes. Hover keeps `card_hover` as the row background, under the fill.

### D5. The grid

- **Columns:** `flex_wrap` (the repo has no `grid()` use) with cards of `w(relative(0.5))` minus the gap. That gives two columns of about 322 px in the 680 px panel after the overlay's 8 px insets, `p_1` padding and an 8 px gap.
- **Card contents:**
  1. title (truncated) and the tone chip;
  2. `agent · workspace · short_time(updated_at)`, using `util::short_time` (`now`, `5m`, `2h`, `3d`, `Oct 31`);
  3. if there is a plan, a 4 px rounded bar (track `p.chip`, fill as in D4 at full alpha) with `done/total`;
  4. up to three entry lines: `current()` then `upcoming(2)`, or `upcoming(3)` with no current entry. Each line has the popover's icon and colour and is truncated to one line;
  5. "+N more" in `dim` when `hidden > 0`.
- **Shared drawing:** the entry icon and colour mapping moves out of the popover into a small helper in `src/ui/kit.rs` (`plan_entry_mark(status, palette)`), used by both.
- **Card height:** fixed by content. The height constant comes from the card with all lines present (`GRID_CARD` ≈ 148 px) so the floor is predictable. Shorter cards are padded with `min_h(GRID_CARD)` so rows line up.

### D6. Layout setting and toggle

- **Setting:** `QuickPrefs.picker_layout: PickerLayout { List, Grid }` with `#[serde(default)]` (List), saved through the existing prefs path.
- **Toggle:** an icon button to the right of the search input; the icon is the target layout. It uses icons already registered in `src/main.rs` (`LayoutGrid`/`List` if present, otherwise the nearest existing pair, decided when building).
- **On switch:** the panel saves the setting and re-runs the height fit.

### D7. Height

- **Floor:** `height_floor` delegates to a pure `picker_height(layout, rows) -> f32`:
  - List: `PICKER_CHROME + rows * PICKER_ROW`, unchanged.
  - Grid: `PICKER_CHROME + ceil(rows / 2) * (GRID_CARD + 8)`.
- **Small displays:** the floor is still limited by `fitted_height` (`0.9 * display`) and the cap. When the grid is taller than the panel, the cards' container gets `overflow_y_scroll()` with a fixed max height (panel height − `top` − search row), so the search row stays in view.
- **Fit:** eight cards need about 122 + 4 × 156 = 746 px, inside the 760 px cap on a display of 1086 px or more. On smaller displays the grid scrolls (scenario "Grid on a small display").

## Risks / Trade-offs

- [The querymt change is not released] → Only sessions open in the app show progress. The UI work and its tests do not depend on it, because tests feed `_meta` JSON directly.
- [The fill on a glass panel is hard to see] → 14% alpha may vanish over a bright backdrop. The alpha is one constant to tune by hand; the contrast test guards only the text, not the fill's visibility.
- [Cards of uneven content] → the fixed `min_h` wastes space for sessions without todos. Accepted for a stable floor; revisit after trying it.
- [Open-session plan and list plan disagree after the session closes] → closing drops the live entity, and the listed plan from the last refresh or `plansChanged` is used. With the querymt change it is at most one notification behind.

## Open Questions

- Fill colour for idle sessions (`dim` vs `accent` at lower alpha). This is to be settled by eye, and it does not change tasks or specs.
