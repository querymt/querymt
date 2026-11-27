# Tasks

## 1. Plans of listed sessions

- [x] 1.1 Add `PlanDigest` (`done`, `total`, `entries`, `is_finished`, `current`, `upcoming`, `hidden`; `fraction` was dropped with the ring in 2.4) and its constructor from a plan list (prefer `querymt-todos`, else the first plan; empty gives `None`) in `src/session.rs` (design D3). Verify with unit tests for scenarios "Progress of a todo plan" and "Empty plan shows no progress", plus `upcoming`/`hidden` on a 6-entry plan.
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

- [x] 2.3 Replace the row fill with a leading progress ring, a check when finished, the in-progress todo in the subtitle and one highlight (design D4, revised after the by-hand check of 2.2: the fill read as a second selection and its square corners stuck out of the rounded row; a row-wide bar and a short bar were tried and dropped for the ring). Verify with: quick tests `list_rows_lead_with_a_ring_and_show_the_count_of_the_todos_done` (ring before the title, check when finished, todo in the subtitle, titles in one column, same height, no fill or bar), `a_list_without_progress_keeps_no_place_for_a_ring` and `the_pointer_moves_the_list_cursor_so_there_is_one_highlight`; the hud unit test `the_progress_ring_is_visible_on_a_selected_row` (3:1 over every chrome's lightest backdrop). Superseded by 2.4 after the by-hand check.

- [x] 2.4 Replace the ring and count with the trailing sectioned bar (design D4, second revision): one section per todo in colour while live (accent, active section breathing; warning colour and still while waiting; a sliding segment for a running session without todos), dim grey and still once idle; the subtitle with the folder's last two parts, the project and a branch mark for a worktree, the agent only in a mixed list; the time on idle rows; "Needs input" as the only chip; no motion under reduced motion or Low Power Mode. Remove `progress_colour`, its contrast test and `PlanDigest::fraction`. Verify with:
  - `a_bar_has_a_section_per_todo_and_says_whether_the_session_is_live` (running, running between todos, running without todos, waiting, idle with and without todos, on `live_bar`; "Needs input" only on the waiting row);
  - `the_pulse_breathes_and_never_goes_out`;
  - `with_motion_off_the_bars_are_still_and_ask_for_no_frames` (scenario "Reduced motion", on `bar_motion`; idle lists ask for no frames either);
  - `list_rows_put_the_bar_beside_the_title_and_say_what_a_live_session_does` (scenarios "Live session", "Session without todos", "Idle session with todos": bar after the title, todo in the subtitle only while live, times in one column, rows the same height);
  - `a_worktree_row_names_its_project_and_only_a_branch_a_person_named` (scenario "Worktree session");
  - `a_list_without_bars_keeps_no_place_for_one` (scenario "No session has a bar");
  - `progress_moves_while_the_picker_is_open` and `a_card_shows_the_todos_a_session_is_at`, rewritten for the bar;
  - mutation checks: showing the todo on idle rows, always keeping the bar slot, naming generated branches, and moving with motion off each fail one of these tests.

  Result (macOS, by hand, dev copy against a live agent with 231 sessions): the bar, the idle greys, the time column and the folder line approved. Scenario "Waiting session" is covered by the `live_bar` test, not checked by hand.

## 3. Layout switch

- [x] 3.1 Add `PickerLayout { List, Grid }` and `QuickPrefs.picker_layout` with `#[serde(default)]` (design D6). Verify by extending `old_prefs_load_with_quick_defaults` to cover scenario "Old settings", and with a round-trip test of the saved value.
- [x] 3.2 Add the toggle next to the search input. It saves the setting and re-fits the height. Verify with a quick test for scenario "Layout remembered": toggle, close, reload prefs from the saved file, reopen, and assert the grid.

  Result (macOS, by hand): the grid is remembered after a full restart, and toggling re-fits the panel height right away in both directions. OK.

## 4. Card grid

- [x] 4.1 Move the plan entry icon and colour mapping into `kit::plan_entry_mark` and use it from the title-bar popover. Verify that the popover renders as before (existing tests, manual check).
- [x] 4.2 Render the grid (design D5): two-column `flex_wrap` cards with title, chip, `agent · workspace · short_time`, the list's bar (originally a bar with `done/total`, replaced in 2.4), current plus upcoming entries, and "+N more". Clicking a card picks its session. Verify with quick tests for scenarios "Card of a session with todos", "Card of a session without todos", "Same order in both layouts" and "Pick from the grid".

  Result (macOS, by hand): card with todos (bar, `done/total`, current then pending up to three lines, "+N more"), card without todos, finished plan as a full success-colour bar, same order as the list, and clicking a card to open its session all OK. Search filters the grid as it does the list (not in the spec).

- [x] 4.3 Replace the row arithmetic in `height_floor` with a pure `picker_height(layout, rows)`, and make the grid scroll under the search row when it is taller than the panel (design D7). Verify that `open_picker_raises_the_height_floor` passes unchanged, and add tests for scenarios "Grid in a compact panel" (fake display of 1200 px) and "Grid on a small display" (fake display of 800 px: the panel stops at its cap and the search row stays visible).

## 5. Integration checks

- [x] 5.1 Run `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo fmt --all -- --check`, the wasm build, and `openspec validate quick-picker-progress --type change --strict --no-interactive`.

  Result (final, after the sectioned bar): 743 tests pass; clippy and fmt clean (`-p querymt-gpui`); `crates/querymt-gpui/web/build.sh --release` (the build CI's "Web build" job runs, with wasm-bindgen 0.2.130) succeeds and its artifact check passes (`dist/index.html`, `querymt_gpui.js`, a non-empty `querymt_gpui_bg.wasm` with the wasm magic bytes); both changes validate strictly. The wasm build has one dead-code warning, `kit::tip_with_key`, whose only callers are in the desktop-only quick panel; it is not from this change.
- [x] 5.2 Against a querymt agent built from `add-session-list-plans`, check by hand on macOS:
  - a session that is not open shows its progress in both layouts;
  - its progress moves mid-turn;
  - an open session follows its live plan;
  - the grid fits or scrolls as specified.

  Note the result here, and whether the grid or the list should become the default.

  Results (macOS, by hand):
  - a session that is not open shows its progress in both layouts: OK.
  - the grid fits: OK (all eight cards visible without scrolling). The scroll case needs a display shorter than about 1086 px, so it was not checked by hand; the quick test "Grid on a small display" covers it. The spec stays as is: at most eight sessions, scrolling only on small displays.
  - an open session follows its live plan: OK in both layouts (picker count matches the title-bar popover and moves during the turn).
  - progress moves mid-turn for a session that is not open: accepted. Not watched by hand: no session on the agent was running when it was checked (250 listed, none running), and starting a turn only to watch it was not done. The two halves are covered separately:
    - the agent announces a plan change to a connection that does not follow the session (`querymt-agent-runtime` `control::notifications::plans_changed_tests`, 13 tests, all pass);
    - the app patches the listed summary in place without a `session/list` request, and the open picker follows (store test for scenario "Progress moves while a background session runs"; `progress_moves_while_the_picker_is_open`).
  - default layout: the list stays the default. The grid has not been preferred after use, and its cards do not yet show the list's folder and worktree mark.

## Workflow follow-up

- Archive after `add-quick-session-panel`, so the `quick-panel` requirements are added to the main spec it creates.
- If the grid is preferred after 5.2, open a small change to make it the default.
- Showing progress in the main window's sidebar is a candidate follow-up using `plan_digest` and `live_bar`.
- The grid card's meta line does not yet use the list's folder tail and worktree mark (design D5, "Not yet").
- Filling the room under a short list with more rows, without resizing the window, is open (design, Open Questions).
- Grid keyboard navigation (deferred, not in this change's spec): only Up/Down work in the grid and they move through the cards in reading order, snaking across the two columns; Left/Right do nothing. Enter opens the selected card. The grid needs more work overall, so 2-D arrow navigation is left to a later grid change and is not an acceptance criterion here.
