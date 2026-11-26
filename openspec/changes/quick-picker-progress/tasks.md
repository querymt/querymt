# Tasks

## 1. Plans of listed sessions

- [x] 1.1 Add `PlanDigest` (`done`, `total`, `entries`, `fraction`, `current`, `upcoming`, `hidden`) and its constructor from a plan list (prefer `querymt-todos`, else the first plan; empty gives `None`) in `src/session.rs` (design D3). Verify with unit tests for scenarios "Progress of a todo plan" and "Empty plan shows no progress", plus `upcoming`/`hidden` on a 6-entry plan.
- [x] 1.2 Add `plans: Vec<ListedPlan>` to `SessionSummary` and parse `_meta["querymt/plans"]` in `from_list_entry`, keeping only `items` (design D1). Verify with store unit tests for scenarios "Plan read from the session list", "Agent without plans in its list", "Unknown plan type is ignored" and "Malformed plans keep the session".
- [x] 1.3 Handle `querymt/session/plansChanged` in the notification `match` of `src/store.rs` before the refresh check, patching the listed summary in place (design D2). Verify with tests that feed the notification JSON for scenarios "Progress moves while a background session runs", "Plan emptied" and "Notification for an unknown session", and assert that no `session/list` request is sent.
- [x] 1.4 Add `AgentsStore::plan_digest(&summary)`, preferring a live open session's non-empty `view.plan` (design D3), and switch the title-bar popover's `done/total` to `PlanDigest`. Verify with tests for scenarios "Live plan wins over a stale list" and "Open session without a live plan yet" using an open session from the test agent; the existing session view tests must still pass.

## 2. Progress in the list layout

- [x] 2.1 Add `plan: Option<PlanDigest>` to `PickerRow`, filled in `picker_rows` through `plan_digest`. Verify that `picker_orders_waiting_then_running` passes unchanged and that a new test `picker_rows_carry_the_listed_plan` checks the digest of a listed session.
- [x] 2.2 Draw the fill and the `done/total` count in `render_picker` (design D4). Verify with:
  - quick tests for scenarios "Half-done session", "Session without todos", "Finished plan" and "Progress moves while the picker is open", asserting the count text through `debug_selector`s and the fill fraction on the row's data;
  - a unit test that `p.text` keeps a 4.5:1 contrast over the fill blended on `p.surface` for both HUD palettes;
  - a manual check on macOS over a bright and a dark backdrop.

  Result (macOS, by hand): fill, `done/total` count, no-plan rows, the finished plan in the success colour, text readability over bright and dark backdrops, and the hover highlight under the fill all OK. Live progress while the picker is open is covered by the quick test only and was not checked by hand. "Plan emptied" (fill and count go away) was not checked by hand; the store test covers it.

## 3. Layout switch

- [x] 3.1 Add `PickerLayout { List, Grid }` and `QuickPrefs.picker_layout` with `#[serde(default)]` (design D6). Verify by extending `old_prefs_load_with_quick_defaults` to cover scenario "Old settings", and with a round-trip test of the saved value.
- [x] 3.2 Add the toggle next to the search input. It saves the setting and re-fits the height. Verify with a quick test for scenario "Layout remembered": toggle, close, reload prefs from the saved file, reopen, and assert the grid.

  Result (macOS, by hand): the grid is remembered after a full restart, and toggling re-fits the panel height right away in both directions. OK.

## 4. Card grid

- [x] 4.1 Move the plan entry icon and colour mapping into `kit::plan_entry_mark` and use it from the title-bar popover. Verify that the popover renders as before (existing tests, manual check).
- [x] 4.2 Render the grid (design D5): two-column `flex_wrap` cards with title, chip, `agent · workspace · short_time`, bar with `done/total`, current plus upcoming entries, and "+N more". Clicking a card picks its session. Verify with quick tests for scenarios "Card of a session with todos", "Card of a session without todos", "Same order in both layouts" and "Pick from the grid".

  Result (macOS, by hand): card with todos (bar, `done/total`, current then pending up to three lines, "+N more"), card without todos, finished plan as a full success-colour bar, same order as the list, and clicking a card to open its session all OK. Search filters the grid as it does the list (not in the spec).

- [x] 4.3 Replace the row arithmetic in `height_floor` with a pure `picker_height(layout, rows)`, and make the grid scroll under the search row when it is taller than the panel (design D7). Verify that `open_picker_raises_the_height_floor` passes unchanged, and add tests for scenarios "Grid in a compact panel" (fake display of 1200 px) and "Grid on a small display" (fake display of 800 px: the panel stops at its cap and the search row stays visible).

## 5. Integration checks

- [x] 5.1 Run `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo fmt --all -- --check`, the wasm build, and `openspec validate quick-picker-progress --type change --strict --no-interactive`.
- [ ] 5.2 Against a querymt agent built from `add-session-list-plans`, check by hand on macOS:
  - a session that is not open shows its progress in both layouts;
  - its progress moves mid-turn;
  - an open session follows its live plan;
  - the grid fits or scrolls as specified.

  Note the result here, and whether the grid or the list should become the default.

  Results so far (macOS, by hand):
  - a session that is not open shows its progress in both layouts: OK.
  - the grid fits: OK (all eight cards visible without scrolling). The scroll case needs a display shorter than about 1086 px, so it was not checked by hand; the quick test "Grid on a small display" covers it. The spec stays as is: at most eight sessions, scrolling only on small displays.
  - an open session follows its live plan: OK in both layouts (picker count matches the title-bar popover and moves during the turn).
  - still to check: progress moves mid-turn for a session that is not open.
  - default layout: undecided; the list stays the default for now.

## Workflow follow-up

- Archive after `add-quick-session-panel`, so the `quick-panel` requirements are added to the main spec it creates.
- If the grid is preferred after 5.2, open a small change to make it the default.
- Showing progress in the main window's sidebar is a candidate follow-up using `plan_digest`.
- Grid keyboard navigation (deferred, not in this change's spec): only Up/Down work in the grid and they move through the cards in reading order, snaking across the two columns; Left/Right do nothing. Enter opens the selected card. The grid needs more work overall, so 2-D arrow navigation is left to a later grid change and is not an acceptance criterion here.
