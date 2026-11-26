# Proposal

## Why

Agents tell the client which slash commands a session accepts (ACP `available_commands_update`). QueryMT, for example, offers `/compact` and `/worktree`. gpui ignores these updates: the session core drops them (`agent-session-core/src/updates.rs`, `_ => {}`), and the store never reads them. So the only way to run a command is to already know its name and type it in full.

The model picker also has two problems:
- **No keyboard navigation.** Arrow keys do nothing, and Enter always picks the first model shown. In the delegate dialog, Enter even changes the session model instead of the delegate's route.
- **Weak search.** It only finds models where the whole query appears inside one field. `sonnet 4.5`, `gpt5`, `anthropic sonnet` and `cl son` all find nothing.

The other selects use a hand-written matcher (`util::fuzzy_score`) that behaves differently from the model search.

## What Changes

- **Session core (QueryMT, `agent-session-core`):**
  - The header gains `available_commands`, the session's advertised slash commands.
  - Each `available_commands_update` replaces the list in full, as the ACP spec describes. A load snapshot does not clear it.
  - A golden case covers both behaviours.
  - The QueryMT catalog sends names without a leading `/`, as in the spec.
  - gpui moves its pin to the QueryMT revision that has these changes.
- **Early command lists are kept.** gpui keeps the latest command list for a session it does not show yet, and applies it when the session opens. This covers the gap between a `session/new`, remote create or fork response and the session view existing. Today the commands sent right after the `session/new` response are lost, and a new session never gets them again.
- **Opening a large session keeps its commands.** A `session/load` response over 256 KB is decoded off the UI thread into a core of its own, which used to replace the view's core and drop the commands applied before it. The background load now continues on the view's own core.
- **Cmd/Ctrl+P lists the open session's slash commands.**
  - Each entry is a two-line row: the command name (no `/`) and the agent's description of it, dimmed. The search matches name, description and hint.
  - Picking a command that takes no input sends it.
  - Picking a command that takes input puts `/name ` in the composer, with the agent's hint as placeholder text.
- **Typing `/` at the start of the session composer shows a command list.**
  - The list narrows as you type.
  - Up/Down move the highlight; Tab or Enter completes the command; Esc closes the list.
  - Arguments are sent as typed.
- **Model picker keyboard navigation.**
  - Up/Down move a highlighted row, which scrolls into view. Enter picks the highlighted model.
  - The highlight starts on the current model.
  - This works in the Start and session picker, the delegate dialog and setup.
  - Enter in the delegate dialog changes the delegate's route, as a click does.
- **Fuzzy search uses `frizbee`.**
  - It replaces the model search's plain substring check and the hand-written `util::fuzzy_score` (profile, target and workspace selects).
  - Every word of a query must match somewhere in the item's fields. Small typos are tolerated, and results are ranked best first.
  - A match in an item's own name ranks above the same match only in a field that describes or locates it (provider, path, description, keywords), so `op` lists `claude-opus-*` before every model of a provider named `openrouter`.
  - In the command palette the groups are ordered by their best match, so `comp` lists the command `compact` before the action "Schedule a prompt…".
- **BREAKING (QueryMT wire, additive for clients that follow the spec):** QueryMT's `availableCommands[].name` no longer starts with `/`. gpui accepts names with or without it.

## Capabilities

### New Capabilities
- `slash-commands`: the open session's advertised slash commands. Covers how gpui receives and keeps them (including updates that arrive before the session is shown), running them from the command palette, and the `/` list in the session composer.
- `model-picker`: choosing a model with the keyboard in the model picker (Start, session, delegate dialog, setup): highlighted row, Up/Down, Enter, what Enter changes, and searching models.
- `picker-search`: how the app's search boxes match and rank items (model picker, profile/target selects, workspace field, slash command lists): fuzzy, every word must match, typo-tolerant, best first.

### Modified Capabilities
None. gpui has no archived specs yet. `session-transcript` (from `use-agent-session-core`) keeps its requirements: the slash commands are read from the core's header like the other header fields, which adds nothing to its turn and transcript requirements.

## Impact

- **QueryMT (separate commits on `feat/agent-session-core`):**
  - `crates/agent-session-core`: `view.rs` (header field and view type), `updates.rs` (apply the update), `snapshot.rs` (a load keeps the list), `testing.rs`/`tests` (golden case).
  - `crates/agent-runtime/src/slash_commands/{acp.rs,builtin.rs}`: names without `/`.
  - Generated typeshare types change: there is a new header field.
- **gpui code:**
  - `src/session.rs`/`src/session/view.rs`: the header field becomes a `SessionView` field; `LoadJob` carries the view's core through a background load.
  - `src/store.rs`: keep the latest early command list per agent and session id until a view for that session opens.
  - `src/ui/keys.rs`: palette group, target and the two-line row.
  - `src/ui/session_view.rs`: composer `/` list.
  - `src/ui/start.rs` (`model_sections`, `model_rows`, `model_search`, `select_model`, `pick_first_model`) and `src/ui/mod.rs`: picker cursor, scroll handle, Enter handling.
  - `src/ui/delegates.rs` and `src/ui/setup.rs` use the shared picker.
  - `src/ui/profile_select.rs`, `src/ui/workspace_select.rs` and `src/util.rs` move to `frizbee`; `fuzzy_score` and `word_score` are removed.
  - `src/search.rs`: `rank_tiered` (primary and secondary fields); `src/ui/keys.rs`: `filter_palette` orders groups by their best match.
- **Dependencies:** `frizbee = "0.13"` (MIT; only an optional `serde` dependency; builds for wasm). No other new crates: the command types come through the core's header.
- **Build:** the `querymt-agent-session-core` revision changes in both `Cargo.toml` tables and `Cargo.lock`. The `cargoLock.outputHashes` entries in `flake.nix` are refreshed.
- **Related changes:**
  - `use-agent-session-core` is still open and pins the same crate. This change moves the pin again (its task 2.1 records how).
  - `add-quick-session-panel` will move the composer into a `SessionPane` and route sessions by `SessionKey`. Whichever change lands second carries the composer `/` list and the early command list over.
