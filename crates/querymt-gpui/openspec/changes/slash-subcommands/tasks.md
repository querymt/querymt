# Tasks

Verification commands: `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`, `cargo fmt --all -- --check`, `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`.

Prerequisite: `slash-command-actions` groups 1 to 3 are done.

## 1. querymt core (separate repository, lands first)

- [ ] 1.1 Add the `WorktreeOp` descriptor table beside the parser in `ext_worktree_ops.rs` and build `/worktree`'s `_meta.querymt.subcommands` from it, using the classification in design D1. Verify: a runtime test that every table entry parses (with sample arguments for `required` ones) and every parser branch has a table entry; a catalog test of the advertised JSON.
- [ ] 1.2 Parse `subcommands` into `SlashCommandView.subcommands` in `agent-session-core`, skipping malformed elements. Verify: core tests for a valid list, a malformed element (skipped), a malformed list (no subcommands) and the `accepts` default from the parent.

## 2. Client

- [ ] 2.1 Bump the `querymt-agent-session-core` revision to the commit with group 1. Verify: `cargo check --locked`; the suite passes unchanged.
- [ ] 2.2 Fan out palette entries per subcommand, omit the parent, and run them by their own arity and deliveries (design D2). Verify: tests `palette_lists_worktree_operations`, `palette_runs_a_subcommand_without_input` and `palette_searches_subcommand_descriptions` (spec: Worktree operations listed, Run a subcommand without input, Search by what it does); a command without subcommands still lists as before.
- [ ] 2.3 Destructive subcommands prefill instead of sending, and show the destructive marker (design D2). Verify: tests `destructive_subcommand_goes_to_the_composer` (Enter and click; nothing is sent; the hint shows) and `destructive_rows_are_marked` (spec: Remove from the palette, Marked in the list).
- [ ] 2.4 Add the second-stage subcommand list to the composer (design D3). Verify: tests in `src/ui/slash.rs`: `subcommand_list_narrows_by_second_word`, `tab_completes_the_subcommand_and_keeps_arguments`, `no_subcommand_list_for_plain_commands`, and that Esc closes it without counting toward the double Esc (spec: Typing a subcommand, Complete a subcommand, Command without subcommands).

## 3. Integration checks

- [ ] 3.1 Run the verification commands and `openspec validate slash-subcommands --type change --strict --no-interactive`. Verify: all pass; record the counts here.
- [ ] 3.2 By hand on macOS against an agent with group 1: `worktree list` from the palette runs at once; `worktree remove` lands in the composer; `/worktree st` lists `status`; an older agent shows the single `worktree` entry. Verify: each noted here pass/fail.

## Workflow follow-up

- Archive after `slash-command-actions`.
