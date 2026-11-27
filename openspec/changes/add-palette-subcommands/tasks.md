# Tasks

## 1. Agent: subcommands in the catalog (`querymt-agent-runtime`)

- [ ] 1.1 Add `Subcommand`, the `subcommands` parameter of `descriptor_meta` (the field is written only when the slice is not empty) and pass `&[]` from `registry_to_acp_update` (design D1). Verify: `cargo check -p querymt-agent-runtime --features remote,oauth,plugin-loaders,api`; test `command_without_subcommands_omits_the_field` in `src/slash_commands/acp.rs` covers user commands and `compact` (slash-commands / Command without subcommands omits the field); the existing `acp.rs` tests pass unchanged.
- [ ] 1.2 Add `WORKTREE_SUBCOMMANDS` and `Builtin::subcommands`, and pass them from `Builtin::entry` (design D2, descriptions from the table). Verify: test `worktree_subcommands_are_advertised` in `src/slash_commands/builtin.rs` reads the entry's `_meta.querymt.subcommands` and checks the ten names in order, each with a non-empty description (slash-commands / Worktree subcommands are advertised).
- [ ] 1.3 Add `NativeCommand::subcommand` (exhaustive) and the test `advertised_subcommands_are_accepted` in `src/agent/handle/ext_worktree_ops.rs`. For every table entry, with minimal arguments, `parse` returns `Ok`, `subcommand()` equals the name, and the table length equals the variant count (design D2) (slash-commands / Advertised subcommands are accepted).
- [ ] 1.4 Add a CHANGELOG.md "Unreleased" entry for `_meta.querymt.subcommands`. Verify: the entry names the field and `/worktree`.
- [ ] 1.5 Checks. Verify: `cargo t-runtime -E 'test(/subcommand|descriptor|acp::tests|native/)'`, `cargo fmt -p querymt-agent-runtime -- --check` and `cargo cl-runtime` pass; record the counts here.

## 2. Session core: reading subcommands (`querymt-agent-session-core`)

- [ ] 2.1 Add `SlashSubcommandView` and `SlashCommandView.subcommands` (`#[typeshare]`, `#[serde(default)]`), and read the list in `updates.rs` `slash_commands` (design D3). Update the `SlashCommandView` literals in `querymt-gpui` (`src/ui/slash.rs`, `src/session/commands.rs` test helpers). Verify: tests `malformed_subcommand_entries_are_dropped` and `agent_without_subcommands` in `tests/live_updates.rs` (slash-commands / Malformed subcommand entries are dropped, Agent without subcommands: the core part); `descriptor_is_read_from_the_catalog` passes unchanged.
- [ ] 2.2 Checks. Verify: `cargo nextest run -p querymt-agent-session-core --features test-support --locked`, `cargo fmt -p querymt-agent-session-core -- --check`, `cargo clippy -p querymt-agent-session-core --all-targets --features test-support --locked -- -D warnings` pass; record the counts here.

## 3. Palette: subcommand rows (`querymt-gpui`)

- [ ] 3.1 Add `PaletteTarget::SlashSubcommand` and build the rows after their parent in `palette_groups`: label, description or reason, dim, keywords (design D4). Verify: tests in `src/ui/keys.rs`:
  - `palette_lists_worktree_subcommands` (slash-commands / Palette lists worktree subcommands);
  - `searching_for_a_subcommand` (slash-commands / Searching for a subcommand);
  - `searching_a_subcommands_description` (slash-commands / Searching a subcommand's description);
  - `agent_without_subcommands` (slash-commands / Agent without subcommands: the palette shows only `worktree…`).
- [ ] 3.2 Picking and Tab prefill `/<command> <subcommand> ` through `prefill_slash`; a blocked row does nothing (design D4). Verify: tests in `src/ui/keys.rs`:
  - `picking_a_subcommand_prefills_it`;
  - `tab_on_a_subcommand_prefills_it`;
  - `subcommand_keeps_the_composer_draft`;
  - `subcommand_during_a_turn` (dimmed with "Available when the turn ends", picking sends nothing, palette stays open).

  They cover the slash-commands scenarios of the same names. The existing tests `tab_adds_arguments_to_an_optional_command`, `key_hint_shows_for_an_optional_input_command` and `tab_on_a_command_without_input_does_nothing` pass unchanged (slash-commands / Adding arguments from the palette, modified).
- [ ] 3.3 Checks. Verify: `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`, `cargo fmt -p querymt-gpui -- --check` and `RUSTC_BOOTSTRAP=1 cargo build --locked -p querymt-gpui --bin querymt-gpui --target wasm32-unknown-unknown` pass; record the counts here.

## 4. Integration checks

- [ ] 4.1 Downstream build. Verify: `cargo t-agent --no-run` builds (querymt-agent re-exports the runtime).
- [ ] 4.2 By hand on macOS, against a `qmtcode` built from this change with `[agent.workspace]` on, in the command palette (Cmd-P):
  - the ten `worktree …` rows follow `worktree…`;
  - `worktree status` finds its row first;
  - Enter on `worktree list…` gives `/worktree list ` and sends nothing;
  - during a turn the rows are dimmed.

  Verify: each item noted here as pass or fail.
- [ ] 4.3 Run `openspec validate add-palette-subcommands --type change --strict --no-interactive`. Verify: it passes.

## Workflow follow-up

- querymt-desktop: regenerate typeshare types for `SlashCommandView.subcommands` (proposal, Impact).
- `add-slash-command-replies` lists "structured subcommands in the catalog and palette" as not planned; point that non-goal at this change.
