# Design

## Context

Agent paths are under `crates/`; app paths are relative to `crates/querymt-gpui`, a member of this workspace that depends on `querymt-agent-session-core` by path.

- **Runtime:** `/worktree`'s operations exist only as a parser in `crates/agent-runtime/src/agent/handle/ext_worktree_ops.rs` (the match on `"create"`, `"list"`, … `"recover"`) and as one hint string in `Builtin::entry`. Submission rules for the whole command are in `Builtin::accepts` (prompt only), advertised as `accepts` by `add-slash-command-records`.
- **Client, after `slash-command-actions`:**
  - `SlashCommandView` carries `source`, `effect`, `input` and `accepts: Option<Vec<..>>` (`add-slash-command-records`).
  - `src/session/commands.rs` `CommandTraits::of` holds the no-descriptor fallback.
  - Palette entries are `PaletteTarget::SlashCommand { name, input, blocked }`; a command the session cannot take is an unavailable row with a reason.
  - Tab puts a command that takes input in the composer.
- **Composer list:** `src/ui/slash.rs` shows the command list while the cursor is in the first word (`slash_list`), moves with `slash_move` (`:117`), completes with `slash_complete_highlighted` (`:134`) and closes with `close_slash_list` (`:204`). Completion replaces the first word and keeps the rest.

## Goals / Non-Goals

**Goals:**
- The advertised subcommands and the agent's parser come from one table, so the catalog cannot list an operation the agent rejects.
- No client knowledge of `/worktree` specifically; any command with subcommands works.

**Non-Goals:**
- Typed argument pickers (for example a worktree id chooser for `status [id]`). The hint stays text.
- Nested subcommands beyond one level.
- A confirmation dialog for destructive subcommands sent from the composer. The composer send is already a deliberate second step; the agent keeps its own conservative removal rules.

## Decisions

### D1. Subcommands in `_meta.querymt.subcommands`

- **Shape:** each element is `{ name, description, input: none|optional|required, hint?, accepts?, destructive: bool }`, with the value names of the catalog descriptor (`add-slash-command-records` D1). `accepts` defaults to the parent's.
- **Runtime source:** a `const` table of `WorktreeOp` descriptors (name, description, input need, hint, destructive) next to the parser in `ext_worktree_ops.rs`. `Builtin::entry` builds the metadata from that table, and the parser's `match` is tested against the same table, so an operation added to one and not the other fails a test.
- **`/worktree` classification:**

  | Subcommand | Input | Destructive |
  | --- | --- | --- |
  | `list` | none | no |
  | `status` | optional | no |
  | `result` | optional | no |
  | `create` | optional | no |
  | `handoff` | optional | no |
  | `publish` | optional | no |
  | `recover` | required | no |
  | `archive` | optional | yes |
  | `remove` | optional | yes |

- **Client view:** `agent-session-core` adds `SlashCommandView.subcommands: Vec<SubcommandView>`. `SubcommandView.accepts` is the parent's `accepts` when the element has none, so it stays `None` only when the parent has no descriptor either. A malformed element is skipped; a malformed list leaves the command without subcommands, so it behaves as today.
- **App:** `CommandTraits::of_subcommand(&SlashCommandView, &SubcommandView)` applies the same per-field fallback as `CommandTraits::of`; it lives next to it in `src/session/commands.rs`.
- **Alternative considered:** parsing the `|`-separated hint in the client. It is fragile, and it cannot express `destructive`.

### D2. Palette: a target per subcommand; destructive forces the composer

- **Target:** `PaletteTarget::SlashCommand { name, sub: Option<String>, input, blocked: Option<Blocked>, destructive }`. `blocked` is the field of `slash-command-actions` D4, kept so a subcommand row can be unavailable.
- **Entries:** `palette_groups` emits one entry per subcommand, labelled `"{name} {sub}"` with the subcommand's description and keywords (parent name, subcommand name, description). The parent entry is omitted when it has subcommands.
- **Picking:** goes through `run_slash_command(name, sub, input, destructive)`.
  - A blocked row does nothing and keeps the palette open, as in `slash-command-actions` D4; this check comes first, also for a destructive subcommand.
  - A destructive subcommand prefills `/{name} {sub} `.
  - Any other subcommand follows `input` as in `slash-command-actions` D6, so Tab still prefills an optional one.
- **Readiness:** `SessionPane::readiness` (`slash-command-actions` D3) gains an optional subcommand argument; with one, it reads `accepts_queue` from `CommandTraits::of_subcommand`, so `blocked` uses the subcommand's `accepts`. Typed text `/name sub …` resolves the subcommand the same way before `send_session_prompt` checks readiness.
- **Destructive marker:** `IconName::TriangleAlert` in `p.danger` replaces the row's `SquareSlash` icon, and the tooltip begins "Destructive:". The label stays in `p.text`, so it stays readable and does not rely on colour alone.
- **Ordering:** fan-out keeps the agent's order. The palette's group ranking (best match first) is unchanged.

### D3. Composer: a second-stage list in the same machinery

- **Data:** `slash_list` returns a `SlashListKind::{Command, Subcommand { parent }}` with its rows.
  - The subcommand stage is active when the first word exactly matches an advertised command with subcommands, it is followed by whitespace, and the cursor is within the second word.
  - Matching uses the existing name fuzzy matcher on the subcommand names only, as the first stage matches names only.
- **Completion:** replaces the second word with `sub ` and keeps everything after it. It reuses the existing first-word code with a word index.
- **Keys:** `slash_keys` does not change. Esc dismissal tracking already keys on the text and resets on edit.
- **Rows:** `/name` in `muted` and `sub` in `text`, so the stage is visible; the destructive marker as in D2.

## Risks / Trade-offs

- [The palette grows by about ten rows for `/worktree`] → The rows are only listed while a session advertising `/worktree` is open. They rank by search like the rest, and with no query they follow the existing group order and cap.
- [The agent's flag form `create --include-local-changes` is not a subcommand] → It stays reachable through `create`'s hint. Flags are out of scope.
- [A client on an older core sees no subcommands] → It shows today's single `worktree` entry.
