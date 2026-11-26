# Tasks

Verification commands:
- **gpui:**
  - tests: `cargo nextest run --package querymt-gpui --bins --locked`
  - lint: `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
  - wasm: `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`
- **QueryMT (`../querymt`):**
  - tests: `cargo nextest run -p <package> <filter>`
  - lint: `cargo clippy -p <package> --all-targets -- -D warnings`

Groups 1–2 do not depend on the QueryMT pin and can land first.

## 1. Shared matcher on frizbee (gpui, design D8)

- [x] 1.1 Add `frizbee = "0.13"` to `[dependencies]`. Verify: `cargo build --locked` succeeds after `cargo update -p frizbee` writes the lock entry; the wasm build succeeds; `cargo tree -i frizbee` shows only `querymt-gpui`.
- [x] 1.2 Create `src/search.rs` with `rank(query, items, fields)`. It must:
  - lowercase queries and haystacks and turn `-` `_` `.` `/` `:` into spaces;
  - build one `Pattern::new` per word, with `max_typos = len/4` for words of 5+ characters and 0 otherwise;
  - require every pattern to match, using `Matcher::from_patterns`;
  - return matches best first, keeping list order on ties;
  - treat an empty query as matching everything.

  First confirm in a test that `from_patterns` requires every pattern (AND). If it does not, intersect per-word matches instead and record the deviation in this task.

  Done, with one deviation recorded (confirmed by `every_word_must_match`, and forced by `search_matches_name_id_and_description`): `rank` takes `fields: Fn(&T) -> Vec<String>` and matches each word against each field separately instead of one joined haystack. Joining let `cdr` match `def-qmcp` through letters of three different fields, which the picker-search spec ("in at least one of the item's searchable fields") and the existing profile test reject. A word still matches in any field, different words in different fields, and an item's score is the sum of its words' best field scores (`u32`). Design D8's "one haystack per item" is superseded by this.

  Verify tests:
  - `words_match_across_fields`, `separators_do_not_block_a_match` (`sonnet 4.5` vs `claude-sonnet-4-5`, `gpt5` vs `gpt-5`);
  - `typos_only_for_long_words` (`sonet` matches Sonnet, `gpz` does not match `gpt`);
  - `contiguous_ranks_first`, `ties_keep_list_order`, `operators_are_literal` (`^foo`);

  which cover picker-search / all requirements.
- [x] 1.3 Move the profile and target selects (`profile_select.rs` `search`) and the workspace field (`workspace_select.rs`, workspaces and folder names) to `search::rank`. Remove `util::fuzzy_score`, `word_score` and `fuzzy_score_ranks_contiguous_and_earlier_fields_higher`. Verify:
  - `hosts_match_fuzzily_by_name_and_any_part_of_the_peer_id`, `search_matches_name_id_and_description` and the workspace field tests pass unchanged;
  - searching `fuzzy_score` in `src/` finds nothing.

  Note: the matcher reads `.` as a separator, so the workspace folder list applies a leading `.` of the partial name itself (it selects hidden folders), keeping `folders_fuzzy_match_the_partial_name` unchanged.

- [x] 1.4 Field tiers and palette group order (after review; design D8, D5). `search::rank_tiered(query, items, primary, secondary)`: a match in a secondary field counts for half; `rank` is the one-tier case. Callers: model picker (label, bare model name / id, provider, node), profile and target select (name / id, description), workspace field (name / path), palette (label / keywords); the composer slash list and folder names stay one tier. `filter_palette` also orders the groups by their best entry (stable). Reports that led to it: `comp` listed `Schedule a prompt…` above `compact`; `op` listed `thinkingmachines/inkling-2-mini` (openrouter) above `claude-opus-*`. Verify tests (picker-search / Best match first; slash-commands / Palette groups follow the best match):
  - `search.rs`: `a_name_match_beats_a_match_on_the_provider_alone`, `a_secondary_field_alone_still_matches`, `within_a_provider_the_better_name_leads`;
  - `start.rs`: `a_name_match_beats_a_provider_match`, `within_a_provider_the_better_name_leads`;
  - `keys.rs`: `groups_are_ordered_by_their_best_match` (`comp`: Slash commands first, `Schedule a prompt…` still listed below; ties keep order; empty query unchanged), `the_best_match_is_the_first_row`;
  - the existing matcher, picker, profile, workspace and palette tests pass unchanged.

  Checked by mutation: with secondary fields counting in full, four of these fail (both `op` cases in `search.rs` and `start.rs`); without the group sort, `groups_are_ordered_by_their_best_match` fails. `the_best_match_is_the_first_row` does not (the rig has no scheduling agent, so Actions does not match `comp` there); the hand-built groups in the other test carry the `Schedule a prompt…` case. Why the first version missed it: the matcher treated all fields alike and design D5 fixed the group order; the model fixture had no provider whose name begins with a common query, and no palette test typed a query that matches in two groups.

## 2. Model picker keyboard and search (gpui, design D7)

- [x] 2.1 Add `model_entries(cx) -> Vec<PickerEntry>` (`Heading`, `Model`, `More`) built from `search::rank` over label, id, provider and node. While searching:
  - groups are ordered by their best score, and models within a group by score;
  - Recent is hidden;
  - there is no row limit.

  Make `model_rows` render from it. Verify tests:
  - `model_search_cases`: `anthropic sonnet`, `sonnet 4.5`, `gpt5` with `gpt-5…` first, `cl son` (model-picker / Searching models);
  - existing picker UI tests pass.
- [x] 2.2 Add `model_cursor` and `model_scroll`:
  - `MoveUp`/`MoveDown` capture actions on `model_search`'s field move the cursor over `Model` entries, wrapping, scroll to the entry's index and stop propagation;
  - `.track_scroll` on the modal, delegate and setup lists;
  - the highlighted row gets a highlight style.

  Verify `TestAppContext` tests `down_skips_headings_and_wraps`, `highlight_scrolls_into_view`, `hidden_rows_are_not_reachable` (model-picker / Arrow keys move the highlight).
- [x] 2.3 Cursor resets:
  - on open (`open_model_picker`, `pick_delegate`), the cursor goes to the selected model's index, else 0;
  - on a `model_query` change, it goes to 0;
  - it is clamped on render.

  Verify tests `picker_opens_on_the_current_model`, `search_highlights_the_first_result` (model-picker / Highlighted model row).

  Note: `model_cursor` is the position among model rows (headings and "more" lines are not counted); `model_seen_query` makes the `Change` handler reset the cursor only when the text really changed, so the reset done on open is not undone by the event the field emits for it. Setup also resets the cursor in `setup_go`.
- [x] 2.4 Add `pick_model(key)` (delegate route if editing one, else `select_model`) used by row clicks and by the `PressEnter` subscription through `pick_highlighted_model`; remove `pick_first_model`. Verify tests:
  - `enter_picks_the_highlighted_model`, `enter_with_no_results_does_nothing`;
  - `enter_in_delegate_dialog_routes_the_delegate` (the session model is unchanged);
  - `enter_on_start_sets_the_launch_model`;

  Tests live in `ui/start.rs` (`model_picker_tests`, on `store::launch::models_fixture`) and `ui/delegates.rs`.

  which cover model-picker / Enter picks the highlighted model and What a pick changes.

## 3. Commands in the session core (QueryMT `agent-session-core`, design D1)

- [x] 3.1 Add `SlashCommandView { name, description, input_hint }` (`#[typeshare]`, camelCase) and `SessionHeader.available_commands`. In `apply_update`, map `AvailableCommandsUpdate` to a full replacement:
  - names without leading `/`;
  - entries with an empty name are dropped;
  - duplicate names keep the first;
  - the input hint comes from `Unstructured`.

  Verify unit tests in `tests/live_updates.rs`: `available_commands_replace_the_list`, `empty_update_clears_commands`, `slash_prefix_is_trimmed`, `input_hint_is_kept` (`cargo nextest run -p querymt-agent-session-core available_commands`).
- [x] 3.2 Keep the list across loads: `hydrate_header` and the no-snapshot path of `snapshot_loaded` do not touch `available_commands`. Verify tests in `tests/snapshot.rs`: `snapshot_load_keeps_commands` and `load_without_snapshot_keeps_commands` (slash-commands / Loading a session keeps its commands).
- [x] 3.3 Add the golden case `available_commands` to `testing::golden_cases()`: update, snapshot load, replacement with a `/`-prefixed name and an input, then an empty update. Regenerate traces with `cargo run -p querymt-agent-session-core --example regen-traces --features test-support` and commit the result separately. Verify:
  - `golden_traces_match_the_committed_output` passes;
  - the trace diff outside the new file only adds `availableCommands: []`;
  - `scripts/session-core-native/check.sh` passes;
  - clippy is clean for the package.

## 4. Spec-style command names (QueryMT `agent-runtime`, design D2)

- [x] 4.1 `registry_to_acp_update` and `Builtin::entry` emit names without `/`; add `Builtin::command_name()`; prompt parsing is unchanged. Update the assertions in `slash_commands/acp.rs` tests, `runtime_tests/handle/core_a.rs` (`/root`, `/docs`, `/compact`, `/deploy`), `worktree_slash_commands.rs` and `remote_session_actor_ref.rs` to bare names. Verify:
  - `cargo nextest run -p querymt-agent-runtime slash` passes;
  - `cargo nextest run -p querymt-agent slash_command` and `cargo nextest run -p querymt-agent remote_session` pass;
  - a `/compact` prompt still compacts (existing `core_a` queue test passes);
  - clippy is clean for both packages.
- [x] 4.2 Push groups 3–4 to `feat/agent-session-core`. Verify: `git ls-remote https://github.com/querymt/querymt` lists the sha as the branch tip; record the sha here.

  Pushed: `a8895c5a0d918476502e33c243ede5ea7e707718` (`git ls-remote https://github.com/querymt/querymt refs/heads/feat/agent-session-core` lists it as the tip). The commits are `c15fe848` (core header field and tests), `3db733c2` (regenerated golden traces only) and `a8895c5a` (bare command names).

## 5. Pin the new QueryMT revision (gpui)

- [x] 5.1 Set `rev` to the 4.2 sha in both `querymt-agent-session-core` entries of `Cargo.toml`; update `Cargo.lock` with `cargo update -p querymt-agent-session-core`; refresh `flake.nix` `cargoLock.outputHashes` with the NAR-hash procedure of `use-agent-session-core` task 2.1, and note the new pin there. Verify:
  - `cargo build --locked`, the wasm build and all gpui tests (including `differential_fixtures`) pass;
  - `cargo tree -i tokio -e normal --target wasm32-unknown-unknown` is empty;
  - where Nix is available, `nix build` passes (otherwise record that it was not run).

  Done at `a8895c5a0d918476502e33c243ede5ea7e707718`:
  - `rev` is set in both `Cargo.toml` entries; `Cargo.lock` moved only the three `querymt` git sources (`querymt`, `querymt-agent-protocol-types`, `querymt-agent-session-core`) and gained `frizbee` (task 1.1). `cargo update` also re-pointed `iana-time-zone`'s `windows-core` edge from 0.58.0 to 0.57.0 (both were already locked); that was reverted by hand to keep the diff to this change, and `--locked` accepts it.
  - `flake.nix` `outputHashes` holds `sha256-iUprdJSzgu4zwq7xQRJzdMnQ1wEDWb38YAEGRtQbTI0=` for all three entries. Computed without Nix with `source_nar_hash` of querymt's `update-desktop-pin` script (`REPO=querymt/querymt`); the same run reproduced the previous pin's `sha256-KOIjIKL+…` for `485919a8…`.
  - `cargo build --locked`, `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown` and `cargo nextest run --package querymt-gpui --bins --locked` (241 passed, 4 `#[ignore]`d) pass. The test named `differential_fixtures` in this task is now `session::golden::tests::golden_fixtures`; it and `core_cases_project_consistently` pass, the latter over the new `available_commands` case.
  - `cargo tree -i tokio -e normal --target wasm32-unknown-unknown` prints nothing.
  - NOT run: `nix build` (Nix is not installed here). If it reports a hash mismatch, replace the value with the one Nix prints in all three entries.

## 6. Commands in gpui's session view and store (design D3, D4)

- [x] 6.1 Map `available_commands` in `map_header`/`HeaderFields`; `refresh_header` always sets `SessionView.slash_commands`; add `Store::session_slash_commands()`. Verify `view_tests.rs` tests (slash-commands / Advertised commands follow the agent):
  - `commands_follow_the_latest_update`;
  - `empty_update_clears_commands`;
  - `commands_of_another_session_are_ignored`.

  Done: `HeaderFields.slash_commands` (`projection.rs`), `SessionView.slash_commands` set in `refresh_header` and copied by `shallow()` (so chrome rendered from a shallow copy sees them), `AgentsStore::session_slash_commands()`; the three tests are in `view_tests.rs`. The helper has no caller until section 7 (one `dead_code` warning until then).
- [x] 6.2 Add `Store.early_commands`:
  - fill it in `on_incoming` with the latest `available_commands_update` for a session without a view (at most 32 entries, oldest dropped);
  - clear it per agent when the connection generation changes;
  - drain it into the view with `queue_session_update` when `launch` and `open_session` create the view, before other inputs.

  Verify tests (slash-commands / Commands advertised before the session is shown):
  - `commands_sent_before_the_launch_view_are_applied` (`session/new` answer, commands, then the view);
  - `early_commands_are_bounded`;
  - `early_commands_survive_a_load_response` (open, commands, then the load response with a snapshot).

  Done in `src/store/early_commands.rs` (`EarlyCommands`, `AgentsStore::deliver_session_update`, `take_early_commands`, tests there). `on_incoming` routes `session/update` through `deliver_session_update`; `connect` and `disconnect` (the two places `generation` changes) call `clear_agent`; `launch` takes the entry right after `SessionView::new`, before `prompt_optimistic`, and `open_session` right after `SessionView::new`. Extra tests: `the_latest_early_update_wins`, `early_commands_stay_with_their_session_and_agent`, `only_commands_are_buffered`, `a_new_connection_forgets_the_agents_early_commands`, `a_view_for_the_session_gets_updates_directly`. Not covered by a test: the `launch`/`open_session`/`connect` call sites themselves (they need a gpui `Context` and a live agent); the tests drive the same helpers they call.

- [x] 6.3 Background loads keep the view's core (design D9). `begin_load` moves the view's core into the `LoadJob` (an empty placeholder stays until `finish_load`); `hydrate` applies the snapshot to it. Also in `open_session`, take the early commands only after the checks that can still return. Verify tests (slash-commands / Loading a session keeps its commands, Large session loaded in the background and Commands known before the session is opened):
  - `background_load_keeps_the_commands_advertised_before_it` and `background_load_applies_a_later_advertisement_on_top` (`view_tests.rs`);
  - `early_commands_survive_a_background_load` (`early_commands.rs`): an advertisement made while the load is in flight, and one made before the view existed;
  - `inputs_during_background_hydration_keep_their_order` and `early_commands_survive_a_load_response` still pass.

  Done in `src/session/view.rs` (`LoadJob` now carries the `SessionCore`, not a session id) and `src/store.rs` (`open_session`). **Why it was missed:** tasks 6.1–6.2 only exercised the inline `load` path; responses over `HYDRATE_BYTES` (256 KB, desktop) went through `begin_load`/`hydrate`/`finish_load`, which built a new core and dropped the commands that had been applied to the old one. Over a WebSocket the agent sends the list before the load response; on stdio it is usually already queued when the load task runs. Checked by mutation: with `hydrate` building a fresh core again, `background_load_keeps_the_commands_advertised_before_it` and `early_commands_survive_a_background_load` fail. A test that a mode update survives the background load was dropped: it passes either way because the view keeps its own `mode` field, so it proved nothing.

## 7. Command palette (gpui, design D5)

- [x] 7.1 Make `palette_view` collect `(CommandGroup, Vec<PaletteTarget>)` pairs. Add the "Slash commands" group (session page, non-empty list), with `/name` labels, description and hint as keywords, and `PaletteTarget::SlashCommand { name, takes_input }`. Verify tests:
  - `palette_lists_session_commands` and `no_slash_group_without_a_session` (slash-commands / Slash commands in the command palette);
  - `palette_targets_stay_aligned_without_sessions` (with no sidebar sessions, picking a slash command runs it rather than a different target).

  Done in `src/ui/keys.rs`: `PaletteEntry`/`PaletteGroup` (label, icon, keywords and target in one struct), `palette_groups`, `filter_palette`, `PaletteTarget::SlashCommand`. Decision taken with the user: the spec's `free context` cannot match under gpui-kit's whole-query-substring matcher, so the palette now sets `filterable(false)` and filters with `search::rank` (design D5 updated; the existing groups use it too). Extra tests: `typing_in_the_palette_filters_the_rendered_rows` (types a query into the real palette and checks `matched_count`), and the `free context` / `context free` / `instructions` queries inside `palette_lists_session_commands`. `SquareSlash` was added to the web build's icon list in `main.rs`.
- [x] 7.2 In `run_palette`:
  - a command without input sends `/name` through `send_prompt` after the same readiness check as `send_session_prompt`;
  - a command with input sets the composer to `/name `, moves the cursor to the end and focuses it.

  Verify tests (slash-commands / Running a command from the palette and Command names with or without a slash):
  - `palette_sends_a_command_without_input`;
  - `palette_fills_the_composer_for_a_command_with_input`;
  - `palette_command_is_queued_while_running`;
  - `spec_and_prefixed_names_are_sent_with_one_slash`.

  Done: `run_slash_command` in `keys.rs`. The readiness check was extracted from `send_session_prompt` into `session_ready` (`session_view.rs`) so both share it; the typed-prompt path is unchanged. Extra test: `palette_does_not_send_while_the_session_is_not_ready`. A command with input sets the composer with `set_value` then `set_cursor_position` (which also focuses).

- [x] 7.3 Two-line palette rows. The slash entries' label is the bare name (no `/`); `PaletteEntry` gains an optional `description`, and `palette_view` draws a custom row (`CommandItem::child`: icon, name, dimmed description under it, ellipsis) for entries that have one. The description and hint stay as search keywords. Verify tests (slash-commands / Slash commands in the command palette):
  - `palette_lists_session_commands`: labels `compact`, `worktree`; the first entry's description is the agent's text; `free context`, `context free` and `instructions` still find `compact`;
  - `palette_targets_stay_aligned_without_sessions`, `spec_and_prefixed_names_are_sent_with_one_slash` (palette labels are bare, the sent prompts still `/name`);
  - the other palette tests pass with bare labels.

  Done in `src/ui/keys.rs`. The composer list is unchanged (`/name` in mono). The rendered two-line row is not covered by a test (the tests read `PaletteEntry`, not pixels); see 9.2.

## 8. Composer command list (gpui, design D6)

- [x] 8.1 Show the list when the text starts with `/`, the text before the cursor has no whitespace, commands exist, and Esc has not closed it for this text. Match names with `search::rank`; the list is a deferred popover with at most 8 visible rows and row clicks complete. Verify tests (slash-commands / Command list in the session composer and Mouse in the composer command list):
  - `slash_shows_all_commands`, `typing_narrows_by_name` (`/wt` lists only `/worktree`);
  - `slash_inside_text_shows_nothing`, `no_commands_no_list`;
  - `click_completes_and_keeps_focus`.

  Done in `src/ui/slash.rs` (new): `slash_list` derives the list on render (page, composer focus, text, cursor, `slash_dismissed`, ranked names via `search::rank`); `render_slash_list` is a `deferred` popover above the composer, 8 rows of 30 px then scroll (`slash_scroll`, `scroll_to_item`), rows complete on click through `complete_slash`. The composer wrapper in `session_view.rs` is `relative()` for it, and `session_composer` now takes `window`. Extra tests: `arrows_move_the_highlight` (wrap-around, and a new query resets to the first match), `list_shows_at_most_eight_rows`, `completion_keeps_typed_arguments`, and pure tests of `slash_query`/`complete`/`hinted`. Design D6 gained the focus condition.
- [x] 8.2 Keys:
  - `MoveUp`/`MoveDown`, and `Enter`/`IndentInline` (complete, keeping typed arguments), are capture actions active only while the list is shown;
  - Esc goes through `close_topmost` (sets `slash_dismissed`, resets `last_escape`).

  Verify tests (slash-commands / Keyboard in the composer command list):
  - `tab_completes_without_cycling_mode`;
  - `enter_completes_instead_of_sending`;
  - `esc_closes_the_list_and_does_not_stop_the_turn`;
  - `send_after_completion_sends_arguments`;
  - `keys_are_normal_once_closed`.

  Done: `slash_keys` registers `MoveUp`/`MoveDown`/`Enter`/`IndentInline` capture handlers before the composer's own (each acts only while `slash_list` is `Some`; Enter only when the send shortcut would submit). Esc: `close_slash_list` is the first test of the root Esc handler's chain (not inside `close_topmost`, which has no `cx`/`window` and other callers); design D6 updated. Checked by mutation: without the Esc hook or the Tab handler, five of these tests fail.
- [x] 8.3 Show the hint strip when the text is exactly `/name ` for a command with a hint; hide it once arguments are typed. Verify test `hint_shows_until_arguments_are_typed` (slash-commands / Running a command from the palette, Command with input).

  Done: `slash_hint` / `render_slash_hint`, a one-line strip above the textarea; the test covers completion by Tab, the palette path (`run_slash_command`), typing arguments, clearing, and a command without a hint.

## 9. Integration checks

- [x] 9.1 Run the full gpui checks: tests, clippy `-D warnings`, the wasm build. Verify: all three pass.

  Result: `cargo nextest run --package querymt-gpui --bins --locked`: 278 passed, 4 `#[ignore]`d. `cargo clippy --locked --all-targets -- -D warnings`: clean. `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`: builds; `cargo tree -i tokio -e normal --target wasm32-unknown-unknown` prints nothing. Not part of this task, but CI also runs `cargo fmt --all -- --check`: the files of this change are formatted, and it still reports diffs in `session.rs`, `session/golden.rs`, `session/projection.rs`, `session/view.rs`, `store.rs` and `usage.rs`, which already had them before this change's gpui work (checked by stashing).
- [x] 9.1c Re-run after 1.4: `cargo nextest run --package querymt-gpui --bins --locked`: 295 passed, 4 skipped; clippy `-D warnings` clean; `RUSTC_BOOTSTRAP=1` wasm build builds; `search.rs`, `keys.rs` and `start.rs` rustfmt-clean.
- [x] 9.1b Re-run after 6.3 and 7.3.

  Result: `cargo nextest run --package querymt-gpui --bins --locked`: 288 passed, 4 skipped. `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`: clean. `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`: builds (without `RUSTC_BOOTSTRAP=1` it stops in `wasm_thread` with E0554; that is the dependency needing nightly, not this change). `keys.rs`, `view_tests.rs` and `early_commands.rs` are rustfmt-clean.
- [x] 9.2 Manual check against a local QueryMT agent (`qmtcode`). Run with `QMT_DEBUG=1` to see when `available_commands_update` arrives relative to the `session/load` response:
  - start a new session; `/` lists `/compact` and `/worktree`;
  - **open an existing large session (over 256 KB of load response) from the sidebar, without prompting: `/` in the composer and Cmd/Ctrl+P list its commands, on a WebSocket agent and on a stdio agent;**
  - Cmd/Ctrl+P: `comp` lists `compact` first (Enter runs it) with `Schedule a prompt…` below; in the model picker `op` lists the `claude-opus-*` models before models that only match through their provider (e.g. openrouter's);
  - Cmd/Ctrl+P shows each command as a two-line row (bare name, the agent's description dimmed below it; check the row height, the ellipsis on a long description such as `worktree`'s, and the selected-row highlight);
  - Cmd/Ctrl+P → `compact` fills the composer with the hint, Enter sends it, and a compaction appears;
  - open a mesh-hosted session: only the remote built-ins are listed;
  - in the model picker, arrows plus Enter work on Start, in a session, in the delegate dialog and in setup.

  Verify: record each result here (pass/fail).

## Workflow follow-up

- Archive `use-agent-session-core` and this change after review; this change's specs become `openspec/specs/{slash-commands,model-picker,picker-search}`.
- If `add-quick-session-panel` is applied after this change, move the composer list (D6) into `SessionPane` and `early_commands` (D3) into its `SessionKey` routing.
