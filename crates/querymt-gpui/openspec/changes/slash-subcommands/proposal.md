# Proposal

## Why

`/worktree` is ten operations behind one name: `create`, `list`, `status`, `result`, `publish`, `handoff`, `archive`, `remove`, `recover`, plus a flag form of `create`. The app shows it as one command whose input hint is the whole grammar on one line. To run `list`, the user must know it exists, type it, and send. Nothing in the palette or the composer lets them find, search or run a single operation, and nothing separates the harmless ones (`list`, `status`) from the ones that remove or archive work.

## What Changes

- **Core: structured subcommands (querymt).** A command can advertise its subcommands under `_meta.querymt.subcommands`. Each has a name, description, input arity, accepted deliveries, and a `destructive` flag. The `/worktree` built-in advertises all of its operations.
- **Palette fan-out.** A command with subcommands is listed as one entry per subcommand (`worktree list`, `worktree status`, …). They are searchable by the subcommand's name and description, and they run or prefill by the subcommand's own arity, as `slash-command-actions` specifies for commands.
- **Destructive subcommands never run from the palette.** Picking one (`worktree remove`, `worktree archive`) always puts it in the composer and shows its hint, whatever its arity. The row is marked as destructive.
- **Second-stage composer list.** After `/worktree `, the composer lists the subcommands, filtered by the second word, with the same keys as the command list.
- Commands without subcommands are unchanged.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`: subcommands in the palette and the composer, and destructive subcommands kept out of one-step runs.

## Impact

- **querymt (separate repository; must land first):**
  - `agent-runtime/src/slash_commands/builtin.rs` advertises `/worktree`'s subcommands. `ext_worktree_ops.rs` keeps parsing them; the advertised list and the parser need a shared table so they cannot drift.
  - `agent-session-core` parses the list into `SlashCommandView.subcommands`.
- **This repository:**
  - `Cargo.toml` bumps the revision.
  - `src/ui/keys.rs`: palette entries per subcommand.
  - `src/ui/slash.rs`: the second-stage list, and completion of `/name sub `.
  - Tests in both files.
- **Depends on** `slash-command-actions` (arity, deliveries, palette disabled state). Archive after it.
