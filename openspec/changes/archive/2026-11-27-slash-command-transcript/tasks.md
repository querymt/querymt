# Tasks

Paths are relative to `crates/querymt-gpui`. Verification commands (from the workspace root):
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt -p querymt-gpui -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`

Prerequisite: `add-slash-command-records` has landed in this workspace.

## 1. Row model and projection

- [x] 1.1 Add `command: Option<UserCommand>` to `ItemKind::User` and `requested: Option<String>` to `Compaction` (design D1, D2). Set `command: None` and `requested: None` at every existing construction (about 18 `ItemKind::User` sites, mostly tests). No behaviour change. Verify: `cargo check -p querymt-gpui --locked --all-targets` and the full suite pass unchanged.
  - Result: `cargo check -p querymt-gpui --locked --all-targets` passes. `ItemKind::User` had one non-test construction site left (`ui/find.rs` tests); no behaviour change.
- [x] 1.2 Project the records in `src/session/projection.rs`:
  - a user message with a record: `text` is what was typed, `command.expansion` is the block text;
  - a compaction with a record: `requested`.
  Verify: projection tests named after the spec scenarios: `user_defined_command_shows_what_was_typed`, `reloaded_prompt_command`, `compact_with_instructions`, `compact_without_instructions`, `automatic_compaction`, `agent_without_records`, `prompt_that_only_starts_with_a_slash`.
  - Result: all seven named projection tests pass (`cargo nextest run --package querymt-gpui --bins --locked projection::tests`). `reloaded_prompt_command` reloads through a snapshot `prompt_received` event and compares with the live row.
- [x] 1.3 Add the user row's command to `src/session/golden.rs`, and regenerate the golden files with `UPDATE_GOLDEN=1`. Verify: review the diff (only cases with command records may change) and the golden test passes without `UPDATE_GOLDEN`.
  - Result: `Row::UserCommand` is a separate golden row (the name leaves `Row::Command` for `add-slash-command-replies`), so cases without commands print as before; no golden file changed and none needed regenerating (no ported case carries a command record). `golden_fixtures` passes without `UPDATE_GOLDEN`.

## 2. Drawing

- [x] 2.1 Draw the user bubble's command form and the "Show prompt" / "Hide prompt" toggle in `render_item` (design D3). Verify: session view tests `prompt_command_bubble_shows_the_typed_text` and `expanded_prompt_on_demand` (spec: User-defined command shows what was typed, Expanded prompt on demand); a test that Copy copies the typed text.
  - Result: `prompt_command_bubble_shows_the_typed_text`, `expanded_prompt_on_demand`, `a_message_without_a_record_has_no_command_form`, `copy_copies_the_typed_text` pass (`ui::session_view::command_tests`). Find indexes the arguments, the Markdown the bubble draws; `/name` is a separate mono element and is not searchable (design D1/D4 updated). `UserCommand` stores `{ name, arguments, expansion }`. `ui::find::tests::a_command_row_indexes_its_arguments` and `a_command_without_arguments_has_no_view` cover `views_of`.
- [x] 2.2 Show the request on the compaction row and the full instructions in its details (design D2). Verify: session view tests `manual_compaction_names_the_command_and_shows_instructions` and `automatic_compaction_row_is_unchanged` (spec: Compact with instructions, Automatic compaction).
  - Result: `manual_compaction_names_the_command_and_shows_instructions`, `manual_compaction_without_instructions_names_only_the_command`, `automatic_compaction_row_is_unchanged` pass. A manual compaction with instructions but no summary gets a "Show details" / "Hide details" toggle (design D2 updated).
- [x] 2.3 Check by hand on macOS, in the main window and the quick panel, light and dark themes, both densities: a prompt command bubble with the toggle open and closed, and a manual compaction with long instructions. Verify: record the result here.
  - Result: pass. Checked on macOS against a qmtcode built from 2eeac2993 (`add-slash-command-records`), in the main window (comfortable density) and the quick panel (compact density), each in the light and dark themes. The `/explain closures` bubble reads monospace `/explain` then `closures`; Show prompt / Hide prompt opens and closes the expansion box, and Copy copies `/explain closures`. In the quick panel the bubble and the expansion box fit the panel width. A manual compaction with long instructions shows `/compact` and the instructions on one truncated line, and the expanded box shows Instructions (full text), a divider, then the summary. Everything is readable in both themes.

## 3. Consumers

- [x] 3.1 Remove `is_compact_command` from `src/session.rs` with its test and its use in `src/ui/session_view.rs` `activity` (design D4). Verify: a test that an active compaction still gives "Compacting context…", and `rg is_compact_command src` finds nothing.
  - Result: `an_active_compaction_still_reads_compacting_context` and `a_local_compact_prompt_reads_sending_until_the_compaction_starts` pass; `grep -rn is_compact_command crates/querymt-gpui/src` finds nothing.
- [x] 3.2 If `knowledge/sessions.md` or `knowledge/ui.md` lists the transcript's row kinds, add the user row's command form. Verify: the text matches `ItemKind`; if neither file lists them, note that here.
  - Result: `knowledge/ui.md` has no row-kind list; `knowledge/sessions.md` describes the projection, so a bullet on command records was added there and matches `ItemKind` / `Compaction`.

## 4. Integration checks

- [x] 4.1 Run the four verification commands. Verify: all pass; record the counts here.
  - Result: `cargo nextest run --package querymt-gpui --bins --locked`: 833 passed, 7 skipped. `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`: clean. `cargo fmt -p querymt-gpui -- --check`: clean. `RUSTC_BOOTSTRAP=1 cargo build --locked -p querymt-gpui --bin querymt-gpui --target wasm32-unknown-unknown`: builds (one pre-existing dead-code warning on `tip_with_key`; the task command needs `-p querymt-gpui` from the workspace root).
- [x] 4.2 Against an agent built with `add-slash-command-records`, by hand on macOS:
  - a project command from `.qmt/commands` shows as `/name arguments` with "Show prompt", live and after a reload;
  - `/compact keep X` shows a compaction row naming `/compact` and "keep X";
  - an agent without records shows prompts and replies as before.
  Verify: each item noted here as pass or fail.
  - Result:
    - command file: pass, live and after switching away from the session and back. The file was put in `~/.qmt/commands` for the check, because a session in a plain checkout does not see that checkout's `.qmt/commands` (`add-session-project-commands` fixes that).
    - `/compact keep X`: pass. The compaction row names `/compact` and the instructions. The `/compact` prompt bubble stays next to it, as expected until `add-slash-command-replies`.
    - agent without records: not checked by hand; covered by the projection test `agent_without_records` (a message with no record is drawn as before). The "Sending prompt…" label of a local `/compact` is covered by `a_local_compact_prompt_reads_sending_until_the_compaction_starts`.
- [x] 4.3 Run `openspec validate slash-command-transcript --type change --strict --no-interactive`. Verify: it passes.
  - Result: `Change 'slash-command-transcript' is valid`.

## Workflow follow-up

- Archive after `add-slash-command-records`; archive before `add-slash-command-replies`, which extends these rows.
