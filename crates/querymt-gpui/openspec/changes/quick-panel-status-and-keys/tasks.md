# Tasks

Verification commands: `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`, `cargo fmt --all -- --check`, `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`.

## 1. Keys

- [x] 1.1 Add `ToggleSessionList` to `actions!` in `src/ui/keys.rs`, bind it to `cmd-k` / `ctrl-k`, and handle it and `NewSession` on the quick panel's root (design D1). Verify: quick tests `cmd_k_opens_and_closes_the_session_list` (from the composer and from the list's search field; focus returns to the composer) and `cmd_n_starts_a_new_quick_chat_in_the_panel` (spec: Open the session list, Close it again, New quick chat from a conversation); a main-window test that `ToggleSessionList` changes nothing there.
- [x] 1.2 Add `kit::shortcut_label` and `kit::tip_with_key`, and replace every hand-written "Cmd/Ctrl+…" in `src/quick/panel.rs` and `compact_model_chip`, including the row digit hints (design D2). Verify: `grep -rn "Cmd/Ctrl" src/quick src/ui/session_view.rs` finds only doc comments; a unit test that `shortcut_label(&OpenModelPicker)` is `⌘M` under the macOS test platform (spec: Tooltip on macOS).

## 2. Agent mode

- [x] 2.1 Add `SessionPane::mode_chip` and put it in the compact composer row before the model chip (design D3). Verify: compact tests `compact_composer_shows_the_mode` and `tab_in_the_compact_composer_moves_the_chip` (spec: Mode in a conversation, Tab changes the mode visibly); a test that no chip is drawn when `mode_option()` is `None`; `compact_composer_has_no_options_panel` still passes. *Superseded by `add-quick-session-panel` 7.38.*
- [x] 2.2 Add `Picks.mode` and `LaunchRequest.mode`, use it in `AgentsStore::launch`, draw the chip in the bar, and cycle it with Tab / Shift+Tab in the bar's input (design D4). Verify: quick test `mode_for_the_next_quick_chat_only` asserting the launch request's mode and that `prefs.launch_mode` is unchanged (spec: Mode for the next quick chat); `new_chat` clears it; the bar tests from `add-quick-session-panel` 7.17 still pass.
- [x] 2.3 Manual check on macOS of the bar and the composer with three modes and a long model name: nothing overlaps, and the input keeps at least 240 px. Verify: result and a screenshot noted here.

## 3. Context usage

- [x] 3.1 Extract `kit::context_meter` and `SessionPane::context_usage` from `session_chrome`, with the main window drawing exactly as before (design D5). Verify: the existing session chrome tests pass; a unit test of the thresholds (0.69 normal, 0.71 warning, 0.91 danger) on the meter's colour.
- [x] 3.2 Show the meter in the quick header, with the percentage label from 70% (design D5). Verify: quick tests `header_shows_the_context_ring`, `percentage_shows_when_nearly_full` and `no_ring_without_a_context_size` (spec: Context half full, Context nearly full, Unknown context size).

## 4. Integration checks

- [x] 4.1 Run the verification commands and `openspec validate quick-panel-status-and-keys --type change --strict --no-interactive`. Verify: all pass; record the counts here. Result: nextest 507 passed, 5 skipped, 0 failed; clippy `-D warnings` clean; `cargo fmt --check` clean; wasm32 build finishes with no warnings; `openspec validate` reports the change valid.
- [x] 4.2 By hand on macOS: ⌘K and ⌘N in the panel, tooltips show ⌘ notation, the mode chip cycles with Tab in both states, the ring appears and turns warning and danger on a long session. Verify: each noted here pass/fail.

## Workflow follow-up

- Archive after `add-quick-session-panel`, so the `quick-panel` requirements are added to the main spec it creates.
