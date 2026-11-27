# Tasks

Paths are relative to `crates/querymt-gpui`. Verification commands (from the workspace root):
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt -p querymt-gpui -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked -p querymt-gpui --bin querymt-gpui --target wasm32-unknown-unknown`

## 1. Descriptor and recognition

- [x] 1.1 Check that `add-slash-command-records` has landed in this workspace: `SlashCommandView` has `source`, `effect`, `input` and `accepts: Option<Vec<..>>`, and that change's task 4.1 already fixed the test helper `src/ui/slash.rs` `command`. No revision to bump (path dependency). In `crates/agent-session-core/src/view.rs`, re-export `SlashCommandSource`, `SlashCommandEffect` and `SlashCommandRecord` from `querymt_agent_protocol_types::events` (`pub use`), so the app can name them without a new dependency (design Context, Types). Extend the helper so tests can set a descriptor. Verify: `cargo check -p querymt-gpui --locked --all-targets` passes, `cargo nextest run -p querymt-agent-session-core --features test-support` passes, and the full gpui suite passes with no behaviour change.
  - Done: `add-slash-command-records` is in this workspace (archived). `SlashCommandSource`, `SlashCommandEffect`, `SlashCommandRecord` are `pub use`d from `agent-session-core::view`. Tests set a descriptor on `SlashCommandView` fields directly (`src/session/commands.rs` tests) or through `_meta.querymt` JSON via `session_update` (`src/ui/keys.rs`, `src/ui/slash.rs`); the old `src/ui/slash.rs` `command` helper needed no change. `cargo nextest run -p querymt-agent-session-core --features test-support`: 139 passed.
- [x] 1.2 Add `src/session/commands.rs` with `CommandTraits::of` and `recognise` (design D1). Verify: unit tests `descriptor_states_an_optional_input`, `command_without_a_descriptor` (also asserts `is_prompt()` and `is_action()` are both false), `empty_accepts_is_not_a_missing_descriptor` (`accepts: Some(vec![])` gives `accepts_queue` false), `path_is_not_a_command`, `name_case_must_match` (spec: Descriptor states an optional input, Command without a descriptor, and the recognition half of Path is not a command and Name case must match).
  - Done: `src/session/commands.rs`, tests `descriptor_states_an_optional_input`, `command_without_a_descriptor`, `empty_accepts_is_not_a_missing_descriptor`, `path_is_not_a_command`, `name_case_must_match` (plus `a_name_with_a_slash_is_recognised`) pass.

## 2. Submission and readiness

- [x] 2.1 Replace the leading-slash test in `OpenSession::send` with `recognise`, and add the guard for a command that does not accept queueing (design D2). Verify: tests in `src/session/open.rs` or `src/store/open_tests.rs`: `path_is_steered_into_the_running_turn` and `typed_command_is_queued_not_steered` assert the wire method (spec: Path is not a command, Typed command is queued, not steered).
  - Done: `OpenSession::send` uses `recognise`; tests `path_is_steered_into_the_running_turn` and `typed_command_is_queued_not_steered` are in `src/ui/slash.rs` (they assert the wire method: steering first asks `_querymt/session/runtimeState`, a queue goes straight to `_querymt/session/queue`).
- [x] 2.2 Add `SessionPane::readiness` and `Blocked`, and make `session_ready` and `send_session_prompt` use it; show the reason in the composer's hint row for a blocked command (design D3). Verify: tests in `src/ui/slash.rs`: `typed_prompt_only_command_during_a_turn` (nothing sent, text kept, reason shown) and `typed_prompt_only_command_when_idle` (spec scenarios of the same names); the existing `session_ready` tests pass unchanged.
  - Done: `SessionPane::readiness`, `Blocked` (`src/ui/session_pane.rs`; one extra variant `ReadOnly`, "Session is read-only", which no list can show because a read-only session lists no commands) and the reason in the hint row. `session_ready` had no caller left and no test, so it was removed rather than kept as dead code. Tests `typed_prompt_only_command_during_a_turn`, `typed_prompt_only_command_when_idle` pass.

## 3. Palette

- [x] 3.1 Replace `takes_input` with `input` and `blocked` in `PaletteTarget::SlashCommand`; build both in `palette_groups`; make `run_palette` keep the palette open for a blocked command; make `run_slash_command` submit for `None` and `Optional` and prefill for `Required` (design D4, D6). Verify: tests in `src/ui/keys.rs`:
  - Done; all listed tests pass (`palette_runs_an_optional_input_command`, `palette_fills_the_composer_for_a_command_with_input` with the required-input `explain`, `palette_disables_commands_while_disconnected`, `prompt_only_command_is_unavailable_during_a_turn`, `picking_an_unavailable_command_keeps_the_palette_open`, `palette_does_not_send_while_the_session_is_not_ready`; the two unchanged ones pass). The `two_commands()` fixtures now give `compact` a descriptor (`input: optional`); without one a hinted command is required input by the fallback.
  - `palette_runs_an_optional_input_command` (spec: Command with optional input);
  - `palette_fills_the_composer_for_a_command_with_input`, changed to a required-input command (spec: Command with input);
  - `palette_disables_commands_while_disconnected` (spec: Session not connected);
  - `prompt_only_command_is_unavailable_during_a_turn` (spec: Prompt-only command during a turn);
  - `picking_an_unavailable_command_keeps_the_palette_open` (spec: Picking an unavailable command);
  - `palette_does_not_send_while_the_session_is_not_ready`, rewritten to assert the row is blocked with "Session is loading" and the palette stays open;
  - `palette_sends_a_command_without_input` and `palette_command_is_queued_while_running` pass unchanged (spec: Command without input, Palette command during a running turn).
- [x] 3.2 Draw every slash row with the custom child: `…` for required input, the `Prompt` mark, and the reason in place of the description; add source and `prompt` keywords (design D5). Verify: tests `required_input_rows_end_with_an_ellipsis`, `prompt_commands_are_marked`, `palette_searches_the_source` (spec: Required and optional input side by side, Prompt command is marked, Palette searches the source); `palette_lists_session_commands` passes with the description still on the second line.
  - Done: `required_input_rows_end_with_an_ellipsis`, `prompt_commands_are_marked`, `palette_searches_the_source` pass; `palette_lists_session_commands` passes. A slash row without a description, mark or reason keeps the kit's default row (same look, same height).
- [x] 3.3 Check row layout by hand on macOS at the palette's 560 px width, light and dark themes, with the longest advertised name and a row that has both `…` and `Prompt`: the name truncates before the mark and the row height does not change. Verify: record the result here.
  - Done by hand on macOS, command palette (Cmd-P) at 560 px, build of 5e0015330, with `explain` (user command, `…` and `Prompt`) and a command with a very long name. Light: pass. Dark: pass. The long name truncates before the `Prompt` mark, row heights match the `compact` row, and the mark is readable.
- [x] 3.4 Handle Tab on the palette card and add the footer key hint with `Command::footer` (design D6). Verify: tests `tab_adds_arguments_to_an_optional_command`, `tab_on_a_command_without_input_does_nothing`, `key_hint_shows_for_an_optional_input_command` (spec: Adding arguments to an optional-input command, Tab on a command without input, Key hint for an optional-input command); a test that Tab on an Actions row changes nothing.
  - Done: tests `tab_adds_arguments_to_an_optional_command`, `tab_on_a_command_without_input_does_nothing` (includes an Actions row), `key_hint_shows_for_an_optional_input_command` pass. Tab is bound to the search field's `IndentInline` action, which runs before key-down listeners, so the palette card captures that action as well as the key-down. The footer reads the highlight from the `CommandState` it is given at render, so `Command::on_select` is not needed.
- [x] 3.5 Keep the composer's draft when prefilling (design D7). Verify: test `composer_draft_becomes_the_arguments` in `src/ui/keys.rs` (spec scenario of the same name), and `completion_replaces_the_first_word_and_keeps_the_arguments` in `src/ui/slash.rs` passes unchanged.
  - Done: `composer_draft_becomes_the_arguments` (`src/ui/keys.rs`) passes; `completion_replaces_the_first_word_and_keeps_the_arguments` passes unchanged.
  - Fixed after review: a draft that starts with a path (`/tmp/build.log is empty`) had its first word replaced, so the path was lost. `prefill_slash` now replaces the first word only when it is `/` plus a word without another `/` (`starts_with_command_word`, design D7). Tests `a_path_draft_becomes_the_arguments` (`src/ui/keys.rs`, spec: Draft that starts with a path) and `a_path_is_not_a_command_word` (`src/ui/slash.rs`) pass.
- [x] 3.6 If `README.md` or `knowledge/ui.md` describes picking slash commands in the palette, update it: Enter runs, Tab adds arguments, unavailable commands show a reason. Verify: the text matches the spec delta; if neither file mentions it, note that here.
  - Done: neither `README.md` nor `knowledge/ui.md` describes picking slash commands in the palette (`knowledge/ui.md` only names `src/ui/slash.rs`); nothing to update.

## 4. Integration checks

- [x] 4.1 Run the four verification commands. Verify: all pass; record the counts here.
  - Done: `cargo nextest run --package querymt-gpui --bins --locked`: 854 passed, 7 skipped; 856 passed, 7 skipped after the path-draft fix. `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`: clean. `cargo fmt -p querymt-gpui -- --check`: clean. wasm32 build: finished (one existing warning, `tip_with_key` unused, also present without this change).
- [x] 4.2 Against a querymt agent built with `add-slash-command-records`, by hand on macOS:
  - `compact` runs on Enter and Tab puts `/compact ` in the composer;
  - a user command shows `…` and `Prompt`. Put the file in `~/.qmt/commands` unless `add-session-project-commands` has landed: until then a session does not read its own folder's `.qmt/commands`;
  - `worktree…` is unavailable during a turn, in the palette and when typed. `/worktree` is only advertised when managed worktrees are on (`[agent.workspace]` in the agent config); the embedded `qmtcode` config has them off;
  - `/tmp/x is broken` can be steered into a running turn;
  - an agent without the descriptor behaves as before.
  Verify: each item noted here as pass or fail.
  - Done by hand on macOS in the command palette (Cmd-P; the composer's inline `/` popup is a different control and has no marks), build of 5e0015330 against `qmtcode` built after `add-slash-command-records`:
    - `compact`: pass. The footer shows `Enter to run` and `Tab to add arguments` on `compact` and not on `explain`. Enter runs it at once. Tab puts `/compact ` in the composer with the caret at the end and sends nothing.
    - user command shows `…` and `Prompt`: pass (`explain` in `~/.qmt/commands`).
    - `worktree…`: pass. Idle: Enter prefills `/worktree `. During a turn the row is dimmed with a reason, picking it sends nothing, and typing `/worktree list` is refused.
    - `/tmp/x is broken` is steered into a running turn: pass.
    - agent without the descriptor: not checked by hand, it needs an older agent build. Covered by the test `command_without_a_descriptor`.
    - A user command stays available during a turn: it advertises `prompt` and `queue`, so only prompt-only commands such as `worktree` are blocked.
- [x] 4.3 Run `openspec validate slash-command-actions --type change --strict --no-interactive`. Verify: it passes.
  - Done: `openspec validate slash-command-actions --type change --strict --no-interactive` passes.

## Workflow follow-up

- Archive after `add-slash-command-records`; archive before `add-slash-command-replies`, which builds on `src/session/commands.rs`.
