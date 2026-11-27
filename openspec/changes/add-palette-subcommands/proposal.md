# Proposal

## Why

`/worktree` has ten operations behind one name. The palette shows a single `worktree…` row whose input hint is the whole grammar on one line. To run `list` or `status`, the user has to know the operation exists and type it. This change lets the palette list each operation as its own row, so it can be found by searching. It does this without changing how commands run.

## What Changes

- **Agent: subcommands in the catalog.** A catalog entry may carry `_meta.querymt.subcommands`, a list of `{ "name", "description" }`, next to the existing descriptor fields. `/worktree` advertises its ten operations: `create`, `handoff`, `list`, `status`, `result`, `publish`, `merge`, `archive`, `remove`, `recover`. The list comes from one table, and a test checks that the `/worktree` parser accepts every name in it.
- **Session core reads the list.** Each advertised command exposes its subcommands. An entry without a usable name is dropped, and the list is empty when the agent sends none.
- **The palette lists subcommands.** Under the parent row, the palette shows one row per subcommand, `worktree list…`, with the subcommand's description as its second line. Search matches the parent name, the subcommand name and its description.
- **Picking a subcommand only prefills.** Enter, Tab or a click puts `/worktree list ` in the composer and focuses it. Nothing is sent, even for read-only operations. A row the session cannot take now is dimmed with the parent's reason, as for commands.
- Commands without subcommands are unchanged.

## Non-goals

- Running a subcommand from the palette in one step, including the read-only `list`, `status` and `result`. Prefill-only for now.
- A guard or mark for destructive subcommands (`remove`, `archive`, `merge`). Nothing runs from the palette, so no guard is needed.
- A per-subcommand input need, accepted submissions, or hint. A subcommand row uses its parent's descriptor.
- A second-stage list in the composer after `/worktree `. The composer's own command list is unchanged.
- Subcommands for user-defined commands or for `/agents` (`add-agents-slash-command`). The field is generic, but only `/worktree` sets it here.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`:
  - the catalog descriptor gains the optional `subcommands` list, and `/worktree` advertises its operations;
  - the app reads the list;
  - the palette shows one prefill-only row per subcommand.

## Impact

- **ACP wire shape (additive, not breaking):** `_meta.querymt.subcommands` on `available_commands_update` entries. Clients that ignore unknown `_meta` keys are unaffected.
- **`querymt-agent-runtime`** (CI features `remote,oauth,plugin-loaders,api`, no feature-gated code changes):
  - `src/slash_commands/builtin.rs`: the `/worktree` subcommand table and the catalog entry.
  - `src/slash_commands/acp.rs`: the descriptor.
  - `src/agent/handle/ext_worktree_ops.rs`: a test that the parser accepts every table name.
- **`querymt-agent-session-core`:**
  - public API: new field `SlashCommandView.subcommands` and a new type `SlashSubcommandView`;
  - `SlashCommandView` is `#[typeshare]`, so the typeshare output changes;
  - adding a field breaks struct-literal construction outside the crate. The only constructions outside the crate are the test helpers in `querymt-gpui`, which are updated here.
- **`querymt-gpui`** (paths relative to it):
  - `src/ui/keys.rs`: palette rows, the new target, Enter and Tab;
  - `src/ui/slash.rs`: test helpers;
  - `src/session/commands.rs`: test helpers.
- **querymt-desktop UI follow-up (not planned here):** regenerate typeshare types for `SlashCommandView.subcommands`. No UI change is required there.
- **CHANGELOG.md:** an "Unreleased" entry for the new ACP `_meta` field.
