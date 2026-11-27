# Tasks

Verification commands for group 2 (from the workspace root; app paths are relative to `crates/querymt-gpui`): `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`, `cargo fmt -p querymt-gpui -- --check`, `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`.

Prerequisites: `add-slash-command-records` has landed (group 1 needs its catalog descriptor); `slash-command-actions` groups 1 to 3 are done (group 2 needs them).

## 1. Agent crates (lands before group 2)

- [ ] 1.1 Add the `WorktreeOp` descriptor table beside the parser in `ext_worktree_ops.rs` and build `/worktree`'s `_meta.querymt.subcommands` from it, using the classification in design D1. Verify: a runtime test that every table entry parses (with sample arguments for `required` ones) and every parser branch has a table entry; a catalog test of the advertised JSON.
- [ ] 1.2 Parse `subcommands` into `SlashCommandView.subcommands` in `agent-session-core`, skipping malformed elements, and fix the `SlashCommandView` constructions in `crates/querymt-gpui` (`subcommands: vec![]`). Verify: core tests for a valid list, a malformed element (skipped), a malformed list (no subcommands) and the `accepts` default from the parent (including `None` when the parent has no descriptor); `cargo check -p querymt-gpui --all-targets --locked` passes.

## 2. Client

- [ ] 2.1 Add `CommandTraits::of_subcommand` in `src/session/commands.rs` and the subcommand argument of `SessionPane::readiness` (design D1 App, D2 Readiness). Verify: unit tests for a subcommand with its own `accepts`, one that inherits the parent's, and one under a parent without a descriptor; the suite passes unchanged otherwise.
- [ ] 2.2 Fan out palette entries per subcommand, omit the parent, and run them by their own input need and accepted submissions, with `blocked` set as for commands (design D2). Verify: tests `palette_lists_worktree_operations`, `palette_runs_a_subcommand_without_input` and `palette_searches_subcommand_descriptions` (spec: Worktree operations listed, Run a subcommand without input, Search by what it does); `unavailable_subcommand_row_shows_its_reason` (a subcommand that does not accept queueing, during a turn: dimmed, reason shown, picking keeps the palette open); a command without subcommands still lists as before.
- [ ] 2.3 Destructive subcommands prefill instead of sending, and show the destructive marker (design D2). Verify: tests `destructive_subcommand_goes_to_the_composer` (Enter and click; nothing is sent; the hint shows), `blocked_destructive_subcommand_does_nothing` (the composer is unchanged) and `destructive_rows_are_marked` (spec: Remove from the palette, Marked in the list).
- [ ] 2.4 Add the second-stage subcommand list to the composer (design D3). Verify: tests in `src/ui/slash.rs`: `subcommand_list_narrows_by_second_word`, `tab_completes_the_subcommand_and_keeps_arguments`, `no_subcommand_list_for_plain_commands`, and that Esc closes it without counting toward the double Esc (spec: Typing a subcommand, Complete a subcommand, Command without subcommands).

## 3. Integration checks

- [ ] 3.1 Run the verification commands and `openspec validate slash-subcommands --type change --strict --no-interactive`. Verify: all pass; record the counts here.
- [ ] 3.2 By hand on macOS against an agent with group 1: `worktree list` from the palette runs at once; `worktree remove` lands in the composer; `/worktree st` lists `status`; an older agent shows the single `worktree` entry. Verify: each noted here pass/fail.

## Workflow follow-up

- Archive after `add-slash-command-records` and `slash-command-actions`.
