# Tasks

Verification commands:
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`

Prerequisite: `slash-command-actions` tasks 1.1 and 1.2 are done (pinned querymt revision with `add-slash-command-records`, and `src/session/commands.rs`).

## 1. Row model and projection

- [ ] 1.1 Add `command: Option<UserCommand>` to `ItemKind::User`, `ItemKind::Command` with `CommandState`, and `requested: Option<String>` to `Compaction` (design D1, D2, D4). Set `command: None` and `requested: None` at every existing construction (about 18 `ItemKind::User` sites, mostly tests). No behaviour change. Verify: `cargo check --locked --all-targets` and the full suite pass unchanged.
- [ ] 1.2 Project the records in `src/session/projection.rs`:
  - a user message with a record: `text` is what was typed, `command.expansion` is the block text;
  - `EntryKind::CommandReply`: `ItemKind::Command`, with JSON output indented;
  - a compaction with a record: `requested`;
  - a pending or failed local prompt that names an advertised action command: `ItemKind::Command` in `Running` or `Failed` (design D3).
  Verify: projection tests named after the spec scenarios:
  - `user_defined_command_shows_what_was_typed`, `reloaded_prompt_command`;
  - `worktree_list_and_its_output`, `refused_command`, `reply_stored_by_the_agent_survives_a_reload`, `reply_the_agent_does_not_store_is_gone_after_a_reload`;
  - `compact_with_instructions`, `compact_without_instructions`, `automatic_compaction`, `compact_on_an_empty_conversation`;
  - `pending_action_command`, `pending_prompt_command`, `failed_action_command`;
  - `agent_without_records`, `prompt_that_only_starts_with_a_slash`.
- [ ] 1.3 Add `Row::Command` and the user row's command to `src/session/golden.rs`, and regenerate the golden files with `UPDATE_GOLDEN=1`. Verify: review the diff (only cases with command records may change) and the golden test passes without `UPDATE_GOLDEN`.

## 2. Drawing

- [ ] 2.1 Draw the user bubble's command form and the "Show prompt" / "Hide prompt" toggle in `render_item` (design D5). Verify: session view tests `prompt_command_bubble_shows_the_typed_text` and `expanded_prompt_on_demand` (spec: User-defined command shows what was typed, Expanded prompt on demand); a test that Copy copies the typed text.
- [ ] 2.2 Draw `ItemKind::Command`: the row, its three states and the output block with the density's line cap and Copy on hover (design D5). Verify: session view tests that the row has no prompt-bubble fill (by `debug_selector`), that a `Failed` row shows the alert mark and its message, that a `Running` row shows the spinner, and that long output is clipped with "more".
- [ ] 2.3 Show the request on the compaction row and the full instructions in its details (design D4). Verify: session view tests `manual_compaction_names_the_command_and_shows_instructions` and `automatic_compaction_row_is_unchanged` (spec: Compact with instructions, Automatic compaction).
- [ ] 2.4 Check by hand on macOS, in the main window and the quick panel, light and dark themes, both densities: a prompt command bubble with the toggle open and closed, a command row in each state, a command row with long JSON output, and a manual compaction with long instructions. Verify: record the result here.

## 3. Consumers

- [ ] 3.1 Make `ItemKind::Command` end the scan in `src/quick/activity.rs` `observe`. Verify: test `error_before_a_command_row_is_not_current` (spec scenario of the same name).
- [ ] 3.2 Update `src/ui/session_view.rs` `activity`, the row hash and the row-kind label for `ItemKind::Command`; remove `is_compact_command` from `src/session.rs` with its test and its use. Verify: tests that a `Running` command row gives "Running /worktree…", that an active compaction still gives "Compacting context…", and `rg is_compact_command src` finds nothing.
- [ ] 3.3 If `knowledge/sessions.md` or `knowledge/ui.md` lists the transcript's row kinds, add the command row and the user row's command form. Verify: the text matches `ItemKind`; if neither file lists them, note that here.

## 4. Integration checks

- [ ] 4.1 Run the four verification commands. Verify: all pass; record the counts here.
- [ ] 4.2 Against a querymt agent built with `add-slash-command-records`, by hand on macOS:
  - a project command from `.qmt/commands` shows as `/name arguments` with "Show prompt", live and after a reload;
  - `/worktree list` shows as a command row with indented output and is gone after a reload;
  - `/worktree publish` (or another stored reply) is still a command row after a reload;
  - `/compact keep X` shows one compaction row naming `/compact` and "keep X";
  - `/compact` in an empty session shows a command row with the reply;
  - an agent without records shows prompts and replies as before.
  Verify: each item noted here as pass or fail.
- [ ] 4.3 Run `openspec validate slash-command-transcript --type change --strict --no-interactive`. Verify: it passes.

## Workflow follow-up

- Archive after `slash-command-actions`; both add requirements to `slash-commands`.
