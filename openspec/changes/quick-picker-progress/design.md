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
  - `is_finished()`;
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

### D4. `PickerRow.plan` and the list row's sectioned bar (revised twice by hand)

- **Row data:** `picker_rows` fills `plan: Option<PlanDigest>` through D3. Sorting and truncation are unchanged.
- **Tried and dropped, in order:**
  - a row-wide fill from the left at 14% alpha: it read as a second selection, its square corners stuck out of the rounded row (GPUI's `overflow_hidden` clips to a rectangle), an idle fill was almost the hover colour, and it vanished on bright glass. Rounding it only made it a shorter selection pill;
  - a row-wide bar under the text (8 px taller rows), and a short bar beside a `done/total` count (coarse, not aligned from row to row);
  - a leading `ProgressCircle` ring with a `CircleCheck` once finished, a `done/total` count and a "Running" chip: it did not say whether a session was live, the count was noise, and idle rows said nothing useful.
- **Row layout:** text first (`flex_1`): the title over a subtitle. Then, centred on the row, fixed-width columns: the bar slot (`LIVE_BAR_WIDTH` 56 px), the time (`TIME_WIDTH` 36 px, idle rows only), the "Needs input" chip (waiting only), and the `⌘N` hint.
- **Subtitle:** the agent's name only when the list mixes agents (`RowLook.show_agent`), then `folder_tail(group_cwd())`, the last two path parts. A worktree session's `cwd` is a hashed checkout, so `group_cwd` gives the project (`sourceCwd`); its row gets a `GitBranch` mark, and the branch name only when it does not start with `qmt/workspace/`. A live session adds ` · <first line of the in-progress entry>`, truncated.
- **Bar model:** a pure `live_bar(row) -> Option<LiveBar>` gives one `BarSection` (`Done`, `Active`, `Pending`) per todo, a `BarPulse` (`None`, `Section(i)`, `Whole`) and the flags `waiting`, `idle`, `indeterminate`. Idle without a plan gives `None`; running without a plan gives one indeterminate section.
- **Bar drawing** (`live_bar_element`), 4 px high, rounded at its two ends only so the sections read as one capsule cut in parts; 2 px gaps up to 12 sections, 1 px up to 20, and above 20 (`BAR_SECTIONS_MAX`) one track filled to the share done:
  - running: the raw `accent`, done solid, the active section breathing, pending at 22%;
  - running with no section active: the whole bar breathes;
  - running without todos: a segment 35% of the track slides across it every 1.4 s;
  - waiting: `warn`, still;
  - idle: `dim` at 55% / 35% / 16%, still: the todos as they were left, well under the text.
- **Colour, not lightness-raised:** the panel's accent is used as is. The earlier `progress_colour` raised it to lightness 0.78 for a 3:1 ring; with sections and a still grey idle state, colour and motion carry the meaning together, and the raised accent read as a different colour from the rest of the panel. The contrast test went with it.
- **Motion:** `pulse_alpha(now)` breathes 40% to 100% over 1.5 s, the same phase for every row. A pure `bar_motion(rows, moving, now)` gives the breath, the slide position and whether a frame is wanted; `render_picker` calls `request_animation_frame` only when it says so, which is while some row pulses or slides and motion is allowed. With reduced motion or Low Power Mode the active section rests at 75% (`PULSE_STILL`) and the segment mid-track.
- **Slot:** when any row has a bar (`RowLook.bar_slot`), rows without one keep the slot empty, so bars and times form columns; when none has, there is no slot and titles take the width.
- **Chips:** `picker_tone` gives only "Needs input". Running is the bar's to show; no spinner, no "Running" chip, no count.
- **Highlight:** one, the keyboard's row (`card_hover`). `on_mouse_move` sets the cursor to the row, and rows and cards have no hover colour. A resting pointer therefore never shows a second highlight.
- **Radius:** rows and cards use `chrome::RADIUS - 8` (12 px): the panel's corner less the list's 8 px inset, so corners stay concentric. Rows and cards use the default arrow cursor, as list items do on the desktop.

### D5. The grid

- **Columns:** `flex_wrap` (the repo has no `grid()` use) with cards of `w(relative(0.5))` minus the gap. That gives two columns of about 322 px in the 680 px panel after the overlay's 8 px insets, `p_1` padding and an 8 px gap.
- **Card contents:**
  1. title (truncated) and the tone chip;
  2. `agent · workspace · short_time(updated_at)`, using `util::short_time` (`now`, `5m`, `2h`, `3d`, `Oct 31`);
  3. if the list row would have a bar, the same bar (D4) across the card;
  4. up to three entry lines: `current()` then `upcoming(2)`, or `upcoming(3)` with no current entry. Each line has the popover's icon and colour and is truncated to one line;
  5. "+N more" in `dim` when `hidden > 0`.
- **Shared drawing:** the entry icon and colour mapping moves out of the popover into a small helper in `src/ui/kit.rs` (`plan_entry_mark(status, palette)`), used by both.
- **Card height:** by content. The height constant comes from the card with all lines present (`GRID_CARD` ≈ 148 px) so the floor is predictable. The earlier `min_h(GRID_CARD)` padding was dropped: it left large empty cards for sessions without todos.
- **Not yet:** the card's meta line still uses `row.workspace`, not the list's `folder_tail` and worktree mark.

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
- [Progress on a glass panel is hard to see] → the bar is solid accent where done, and live state is carried by motion as well as colour. The pending and idle sections are faint on purpose: they are context, not the signal.
- [Animation costs power] → frames are requested only while a visible row is live, never under reduced motion or Low Power Mode.
- [The bar slot costs width] → 56 px plus a gap, and only in lists where some session has a bar.
- [Cards of uneven content] → cards are as tall as their content, so a grid row is as tall as its tallest card. The floor still assumes `GRID_CARD`, so a grid of short cards leaves room under it; accepted, the window does not resize to the list.
- [Open-session plan and list plan disagree after the session closes] → closing drops the live entity, and the listed plan from the last refresh or `plansChanged` is used. With the querymt change it is at most one notification behind.

## Open Questions

- Whether to fill the room under a short list with more rows instead of leaving it empty, without resizing the window.
- The card's meta line with the list's folder and worktree mark.
