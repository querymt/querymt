# Tasks

Paths are relative to `crates/querymt-gpui`. Verification commands (from the workspace root):
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt -p querymt-gpui -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`

Prerequisite: `add-slash-command-records` has landed in this workspace.

## 1. Row model and projection

- [ ] 1.1 Add `command: Option<UserCommand>` to `ItemKind::User` and `requested: Option<String>` to `Compaction` (design D1, D2). Set `command: None` and `requested: None` at every existing construction (about 18 `ItemKind::User` sites, mostly tests). No behaviour change. Verify: `cargo check -p querymt-gpui --locked --all-targets` and the full suite pass unchanged.
- [ ] 1.2 Project the records in `src/session/projection.rs`:
  - a user message with a record: `text` is what was typed, `command.expansion` is the block text;
  - a compaction with a record: `requested`.
  Verify: projection tests named after the spec scenarios: `user_defined_command_shows_what_was_typed`, `reloaded_prompt_command`, `compact_with_instructions`, `compact_without_instructions`, `automatic_compaction`, `agent_without_records`, `prompt_that_only_starts_with_a_slash`.
- [ ] 1.3 Add the user row's command to `src/session/golden.rs`, and regenerate the golden files with `UPDATE_GOLDEN=1`. Verify: review the diff (only cases with command records may change) and the golden test passes without `UPDATE_GOLDEN`.

## 2. Drawing

- [ ] 2.1 Draw the user bubble's command form and the "Show prompt" / "Hide prompt" toggle in `render_item` (design D3). Verify: session view tests `prompt_command_bubble_shows_the_typed_text` and `expanded_prompt_on_demand` (spec: User-defined command shows what was typed, Expanded prompt on demand); a test that Copy copies the typed text.
- [ ] 2.2 Show the request on the compaction row and the full instructions in its details (design D2). Verify: session view tests `manual_compaction_names_the_command_and_shows_instructions` and `automatic_compaction_row_is_unchanged` (spec: Compact with instructions, Automatic compaction).
- [ ] 2.3 Check by hand on macOS, in the main window and the quick panel, light and dark themes, both densities: a prompt command bubble with the toggle open and closed, and a manual compaction with long instructions. Verify: record the result here.

## 3. Consumers

- [ ] 3.1 Remove `is_compact_command` from `src/session.rs` with its test and its use in `src/ui/session_view.rs` `activity` (design D4). Verify: a test that an active compaction still gives "Compacting context…", and `rg is_compact_command src` finds nothing.
- [ ] 3.2 If `knowledge/sessions.md` or `knowledge/ui.md` lists the transcript's row kinds, add the user row's command form. Verify: the text matches `ItemKind`; if neither file lists them, note that here.

## 4. Integration checks

- [ ] 4.1 Run the four verification commands. Verify: all pass; record the counts here.
- [ ] 4.2 Against an agent built with `add-slash-command-records`, by hand on macOS:
  - a project command from `.qmt/commands` shows as `/name arguments` with "Show prompt", live and after a reload;
  - `/compact keep X` shows a compaction row naming `/compact` and "keep X";
  - an agent without records shows prompts and replies as before.
  Verify: each item noted here as pass or fail.
- [ ] 4.3 Run `openspec validate slash-command-transcript --type change --strict --no-interactive`. Verify: it passes.

## Workflow follow-up

- Archive after `add-slash-command-records`; archive before `add-slash-command-replies`, which extends these rows.
