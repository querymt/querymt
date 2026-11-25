# Tasks

Verification commands: `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`, `cargo fmt --all -- --check`, `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`.

## 1. querymt core (separate repository, lands first)

- [ ] 1.1 Advertise `_meta.querymt { input, native, accepts }` on every `AvailableCommand`: built-ins from `Builtin` (`/compact`: optional, native, prompt+queue; `/worktree`: required, native, prompt), registry commands derived from template and hint with the `argument-required` frontmatter override (design D1). Verify: runtime unit tests in `slash_commands/acp.rs` and `builtin.rs` assert the three fields per command, including a template with `$ARGUMENTS`, one with a hint only, and one with the override.
- [ ] 1.2 Parse the metadata into `SlashCommandView { input, native, accepts }` in `agent-session-core`, with the fallback for missing `_meta` (design D1). Verify: core tests for an update with metadata, without metadata (hint gives `Required`, no hint gives `None`), and with a malformed `_meta` (falls back, does not drop the command).
- [ ] 1.3 Record `UserMessage.command: Option<CommandRun { name, args, native, output }>` when a prompt invoked a built-in or registry command; for registry commands the row keeps the typed text (design D4). Verify: core tests that `/compact x`, `/worktree list` and a registry command set `command`, and `/tmp is full` does not.
- [ ] 1.4 Make `WorktreeCommandOutput` durable and replay it as the `querymt/command_output` update linked to its user message, not as `AgentMessageChunk` (design D4). Verify: replay test that a reloaded session yields the command output on the same `UserMessage`, and no assistant message.

## 2. Client: metadata and palette

- [ ] 2.1 Bump the `querymt-agent-session-core` revision in `Cargo.toml` (both entries) to the commit with group 1. Verify: `cargo check --locked` after `cargo update -p querymt-agent-session-core`; the full suite passes unchanged.
- [ ] 2.2 Replace `takes_input: bool` with `input: CommandInput` in `PaletteTarget::SlashCommand` and `SessionPane::run_slash_command`; `none` and `optional` send, `required` prefills; required labels end in `…` (design D2). Verify: update `palette_fills_the_composer_for_a_command_with_input` to a required-input command; new tests `palette_runs_an_optional_input_command` and `required_input_rows_end_with_an_ellipsis` (spec: Command with optional input, Command with input, Required and optional input side by side).
- [ ] 2.3 Tab on a highlighted optional-input slash row prefills the composer (capture on the palette card, `CommandState::selected_index`), and the row's tooltip names Enter and Tab (design D2). Verify: test `tab_adds_arguments_to_an_optional_command` (spec: Adding arguments to an optional-input command); test that Tab on an Actions row does nothing.
- [ ] 2.4 Add `SessionPane::command_readiness` and render blocked slash rows with `Disableable::disabled(true)` and the reason as their second line (design D3). Verify: tests `palette_disables_commands_while_disconnected` and `native_command_is_disabled_during_a_run_and_compact_is_not` (spec: Session not connected, Native command during a run); `palette_does_not_send_while_the_session_is_not_ready` is rewritten to assert the row is disabled and the palette stays open.
- [ ] 2.5 Update the composer's slash list to show the same `…` marker for required input, so the composer and the palette agree. Verify: test in `src/ui/slash.rs` that `/review` is listed as `/review…` and completion still inserts `/review `.

## 3. Client: commands in the conversation

- [ ] 3.1 Add `ItemKind::Command { name, args, output, status }` and project `UserMessage.command` into it; fall back to an advertised first word for agents without group 1; skip `/compact` when a compaction follows and pass its args to that compaction (design D4). Verify: projection tests `command_message_becomes_a_command_item`, `unknown_slash_prompt_stays_a_prompt`, `compact_folds_into_its_compaction`, `reloaded_command_keeps_its_output` (spec: Worktree command and its output, Reloaded session, Compact, Prompt that only starts with a slash).
- [ ] 3.2 Render the command row and its output (design D5) in both densities, with the copy action on hover and the output collapsed by default in compact density. Verify: session view tests that the row has no `accent_dim` fill (by `debug_selector`), that the output uses the tool output block and respects `Density::output_lines`, and a manual check on macOS in the main window and the quick panel, light and dark themes, recorded here.
- [ ] 3.3 Show `/compact`'s instructions in the compaction row's details. Verify: test that a compaction with instructions shows them when expanded; settle the design's open question here.
- [ ] 3.4 Update `openspec/specs/slash-commands` scenarios that the delta replaces (only through archive) and note in `README.md`, if it lists slash commands, that Enter runs and Tab adds arguments. Verify: `openspec validate slash-command-actions --type change --strict --no-interactive` passes.

## 4. Integration checks

- [ ] 4.1 Run the verification commands. Verify: all pass; record the counts here.
- [ ] 4.2 Against a querymt agent built with group 1, by hand on macOS: palette `compact` compacts at once; Tab adds instructions; `worktree` is disabled during a run; `/worktree list` shows as a command with its output and survives a reload; an agent without group 1 behaves as before. Verify: each item noted here pass/fail.

## Workflow follow-up

- Archive after `add-quick-session-panel` only if task 3.2's quick panel check depends on it; otherwise independent.
- `slash-subcommands` builds on the metadata of 1.1–1.2.
