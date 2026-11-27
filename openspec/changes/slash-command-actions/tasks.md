# Tasks

Paths are relative to `crates/querymt-gpui`. Verification commands (from the workspace root):
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt -p querymt-gpui -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`

## 1. Descriptor and recognition

- [ ] 1.1 Check that `add-slash-command-records` has landed in this workspace: `SlashCommandView` has `source`, `effect`, `input` and `accepts: Option<Vec<..>>`, and that change's task 4.1 already fixed the test helper `src/ui/slash.rs` `command`. No revision to bump (path dependency). Extend the helper so tests can set a descriptor. Verify: `cargo check -p querymt-gpui --locked --all-targets` passes and the full suite passes with no behaviour change.
- [ ] 1.2 Add `src/session/commands.rs` with `CommandTraits::of` and `recognise` (design D1). Verify: unit tests `descriptor_states_an_optional_input`, `command_without_a_descriptor` (also asserts `is_prompt()` and `is_action()` are both false), `empty_accepts_is_not_a_missing_descriptor` (`accepts: Some(vec![])` gives `accepts_queue` false), `path_is_not_a_command`, `name_case_must_match` (spec: Descriptor states an optional input, Command without a descriptor, and the recognition half of Path is not a command and Name case must match).

## 2. Submission and readiness

- [ ] 2.1 Replace the leading-slash test in `OpenSession::send` with `recognise`, and add the guard for a command that does not accept queueing (design D2). Verify: tests in `src/session/open.rs` or `src/store/open_tests.rs`: `path_is_steered_into_the_running_turn` and `typed_command_is_queued_not_steered` assert the wire method (spec: Path is not a command, Typed command is queued, not steered).
- [ ] 2.2 Add `SessionPane::readiness` and `Blocked`, and make `session_ready` and `send_session_prompt` use it; show the reason in the composer's hint row for a blocked command (design D3). Verify: tests in `src/ui/slash.rs`: `typed_prompt_only_command_during_a_turn` (nothing sent, text kept, reason shown) and `typed_prompt_only_command_when_idle` (spec scenarios of the same names); the existing `session_ready` tests pass unchanged.

## 3. Palette

- [ ] 3.1 Replace `takes_input` with `input` and `blocked` in `PaletteTarget::SlashCommand`; build both in `palette_groups`; make `run_palette` keep the palette open for a blocked command; make `run_slash_command` submit for `None` and `Optional` and prefill for `Required` (design D4, D6). Verify: tests in `src/ui/keys.rs`:
  - `palette_runs_an_optional_input_command` (spec: Command with optional input);
  - `palette_fills_the_composer_for_a_command_with_input`, changed to a required-input command (spec: Command with input);
  - `palette_disables_commands_while_disconnected` (spec: Session not connected);
  - `prompt_only_command_is_unavailable_during_a_turn` (spec: Prompt-only command during a turn);
  - `picking_an_unavailable_command_keeps_the_palette_open` (spec: Picking an unavailable command);
  - `palette_does_not_send_while_the_session_is_not_ready`, rewritten to assert the row is blocked with "Session is loading" and the palette stays open;
  - `palette_sends_a_command_without_input` and `palette_command_is_queued_while_running` pass unchanged (spec: Command without input, Palette command during a running turn).
- [ ] 3.2 Draw every slash row with the custom child: `…` for required input, the `Prompt` mark, and the reason in place of the description; add source and `prompt` keywords (design D5). Verify: tests `required_input_rows_end_with_an_ellipsis`, `prompt_commands_are_marked`, `palette_searches_the_source` (spec: Required and optional input side by side, Prompt command is marked, Palette searches the source); `palette_lists_session_commands` passes with the description still on the second line.
- [ ] 3.3 Check row layout by hand on macOS at the palette's 560 px width, light and dark themes, with the longest advertised name and a row that has both `…` and `Prompt`: the name truncates before the mark and the row height does not change. Verify: record the result here.
- [ ] 3.4 Handle Tab on the palette card and add the footer key hint with `Command::footer` and `Command::on_select` (design D6). Verify: tests `tab_adds_arguments_to_an_optional_command`, `tab_on_a_command_without_input_does_nothing`, `key_hint_shows_for_an_optional_input_command` (spec: Adding arguments to an optional-input command, Tab on a command without input, Key hint for an optional-input command); a test that Tab on an Actions row changes nothing.
- [ ] 3.5 Keep the composer's draft when prefilling (design D7). Verify: test `composer_draft_becomes_the_arguments` in `src/ui/keys.rs` (spec scenario of the same name), and `completion_replaces_the_first_word_and_keeps_the_arguments` in `src/ui/slash.rs` passes unchanged.
- [ ] 3.6 If `README.md` or `knowledge/ui.md` describes picking slash commands in the palette, update it: Enter runs, Tab adds arguments, unavailable commands show a reason. Verify: the text matches the spec delta; if neither file mentions it, note that here.

## 4. Integration checks

- [ ] 4.1 Run the four verification commands. Verify: all pass; record the counts here.
- [ ] 4.2 Against a querymt agent built with `add-slash-command-records`, by hand on macOS:
  - `compact` runs on Enter and Tab puts `/compact ` in the composer;
  - a project command from `.qmt/commands` shows `…` and `Prompt`;
  - `worktree…` is unavailable during a turn, in the palette and when typed;
  - `/tmp/x is broken` can be steered into a running turn;
  - an agent without the descriptor behaves as before.
  Verify: each item noted here as pass or fail.
- [ ] 4.3 Run `openspec validate slash-command-actions --type change --strict --no-interactive`. Verify: it passes.

## Workflow follow-up

- Archive before `slash-subcommands`, which modifies the same requirements.
- `slash-subcommands` holds both its agent work (group 1) and its app work (group 2). Its subcommand descriptors follow the catalog descriptor of `add-slash-command-records` (`input`, `accepts`, same value names).
