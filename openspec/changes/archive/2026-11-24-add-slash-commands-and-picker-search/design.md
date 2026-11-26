# Design

## Context

- **Slash commands on the wire.**
  - Agents send `session/update` with `sessionUpdate: "available_commands_update"` and `availableCommands: [{ name, description, input?: { hint } }]` ([ACP v1](https://agentclientprotocol.com/protocol/v1/slash-commands.md)). Each update replaces the previous list. Commands run as ordinary `session/prompt` text `/name args`.
  - QueryMT sends the list after `session/new`, `session/load`, `session/resume`, fork and attach (`agent-runtime/src/acp/shared.rs`, `available_commands_session_update`). Nothing sends it again later (not after a prompt). Its names carry a leading `/` in older revisions; the pinned one sends them bare (D2).
  - **Order relative to the load response depends on the transport.** For `session/load` the dispatcher puts the list among the notifications and sends them *before* the response (`notify_before_response`, `shared.rs`), so over a WebSocket the list arrives first. On stdio (`stdio.rs`) it is sent right after the response, and the load response is parsed on the UI thread, so the line is usually already waiting in the channel when the load task gets its turn. Either way the list normally reaches the view **before** `begin_load` runs.
  - Remote (mesh) sessions advertise only built-ins that run there.
  - `/agents` (QueryMT change `add-agents-slash-command`) is refused when queued or steered.
- **Session core.**
  - `SessionCore::apply_update` (`agent-session-core/src/updates.rs`) projects `current_mode_update` and `config_option_update` into `SessionHeader` and drops `available_commands_update` (`_ => {}`).
  - `hydrate_header` (`snapshot.rs`) resets run state, subagents and cursors on a snapshot load.
  - The header is `#[typeshare]`, so other clients see its shape.
  - The golden test (`tests/golden_traces.rs`) serializes the whole header, so a new header field changes every trace. Traces are regenerated with the `regen-traces` example.
  - `SessionUpdate` is ACP's `v1` enum, re-exported by `querymt-agent-protocol-types`. `AvailableCommandsUpdate` already skips malformed entries (`VecSkipError`).
- **gpui routing.**
  - `Store::on_incoming` (`src/store.rs`) sends a `session/update` to the view only when `self.session` has the same agent and session id.
  - **Launch.** `launch` clears `self.session` (`store.rs:2821`) and creates the view only after `session/new` and up to three `set_config_option` round trips (`:2919`). It never calls `session/load`, so commands advertised for a new session are dropped and never re-sent.
  - **Fork and worktree.** These open their session through `open_session`, which creates the view before `session/load`. The agent re-advertises after the load.
- **gpui view.**
  - `SessionView::refresh_header` (`src/session/view.rs:189`) copies `Projection::header_fields()` into public fields after each change.
  - **Two load paths.** `SessionView::load` applies `SnapshotLoaded` to the view's own core. A response over `HYDRATE_BYTES` (256 KB, desktop only) goes through `begin_load` → `LoadJob::hydrate` (background) → `finish_load`, which used to build a **new** `SessionCore` and swap it in with `Projection::replace_core`. Everything the old core's header held, the slash commands included, was dropped. The first version of this change only tested the inline path (see D9).
- **Command palette.**
  - `palette_view` (`src/ui/keys.rs:297`) builds `CommandGroup`s with a parallel `targets: Vec<Vec<PaletteTarget>>` indexed by `IndexPath.section`. The Sessions group is added only when non-empty.
- **Session composer.**
  - The composer (`session_view.rs:4872`) wraps a `Textarea` in a div with `capture_action` handlers: `Enter` sends, and `IndentInline`/`OutdentInline` cycle modes.
  - Escape is handled at the root by `on_key_down` → `close_topmost()` (`ui/mod.rs:625`, `:1225`), then by double-Esc-to-cancel.
- **Model picker.**
  - `model_sections` (`start.rs:681`) filters with a lowercase `contains`.
  - `model_rows` (`:1418`) renders headings, at most `GROUP_ROW_LIMIT` (40) rows per group when not searching, and "N more" lines.
  - `model_search` (`:1632`) is the search field shared by the modal (`:1676`), the delegate dialog (`delegates.rs:691`) and setup (`setup.rs:675`).
  - Enter is a `PressEnter` subscription on `model_query` calling `pick_first_model` (`mod.rs:415`), which always calls `select_model`. A row click calls `route_delegate_model` first (`start.rs:1594`).
- **Other searches.**
  - `util::fuzzy_score` (`util.rs:175`) is used by `profile_select.rs:133` and `workspace_select.rs:79,98`.
  - Their keyboard handling (`capture_key_down` / `capture_action(MoveUp/MoveDown/Enter)` with a `ScrollHandle`) is the pattern the model picker follows.

## Goals / Non-Goals

**Goals:**
- The core is the only place that holds a session's commands; gpui reads them like other header fields.
- One matcher for all of the app's own search boxes.
- The model picker's keyboard behaviour matches the profile/target selects.

**Non-Goals:**
- **Structured command input.** ACP defines only unstructured input.
- **Running commands differently from prompts.** No client-side handling of `/compact` and others.
- **Commands for sessions other than the open one** (sidebar, quick panel).
- **Changing the Start page composer.** No session exists there, so it has no commands.

## Decisions

### D1. Commands live in the core's header (QueryMT)
`SessionHeader` gains `available_commands: Vec<SlashCommandView>`.

- **The view type:** `SlashCommandView { name: String, description: String, input_hint: Option<String> }`, `#[typeshare]`, camelCase.
- **`apply_update`:** maps `SessionUpdate::AvailableCommandsUpdate(update)` to a full replacement of the list.
- **Names:** stored without leading `/` (`trim_start_matches('/')`). Entries whose name is empty after trimming are dropped. Duplicate names keep the first.
- **Hint:** `input_hint` comes from `AvailableCommandInput::Unstructured`. Unknown future input kinds are treated as "takes input" with an empty hint, by matching `Some(_)`.
- **Loads:** `hydrate_header` and the no-snapshot path of `snapshot_loaded` leave the list untouched. The list is session metadata the agent pushes, not part of the load snapshot.
- **Change reports:** `header_changed` already reflects the new field through the header comparison.
- **Tests:** a new golden case `available_commands` covers:
  - update, then replacement;
  - a `/`-prefixed name;
  - an entry with input;
  - a snapshot load in between that keeps the list;
  - an empty update that clears it.

  Traces are regenerated. Every trace's header gains `availableCommands: []`. That diff is expected and is reviewed once.

*Alternatives:*
- **Keep the raw ACP list (`Vec<Value>`), like `config_options`.** Rejected: every client would re-implement slash normalization and hint extraction. The header is shared through typeshare, so a typed view serves all of them.
- **Store commands in gpui's store (option B).** Rejected (see proposal): it reintroduces a second reader of `session/update` beside the core, against `use-agent-session-core`'s "core is the only writer".

### D2. QueryMT advertises spec-style names
`registry_to_acp_update` and `Builtin::entry` emit names without `/`. `Builtin::name()` keeps the slash for display and for parsing prompts; a new `Builtin::command_name()` returns the bare name. The prompt side still recognizes `/name …`, so running a command is unchanged.

Tests: `slash_commands/acp.rs` tests and `runtime_tests/handle/core_a.rs` assertions now expect bare names. Those assertions currently use `"/compact"`, `"/root"` and `"/docs"`.

*Alternative:* leave QueryMT as is and normalize only in the core. Rejected: QueryMT would remain off-spec for every other ACP client. The core still trims a leading `/` (D1) for other agents.

### D3. gpui keeps the latest early commands per session
`Store` gains `early_commands`, a small map from `(agent_id, session_id)` to the latest `available_commands_update` params. It is filled in `on_incoming` when a `session/update` of that kind arrives for a session without a view.

- **Only the latest per session.** Each update replaces the list, so older ones are useless.
- **Only `available_commands_update`.** Other early updates are not buffered:
  - Config and mode come from the creation responses the launch already applies. A buffered older `config_option_update` would undo them.
  - Transcript content arrives through load or the first prompt.
- **Bounded:**
  - at most 32 entries, dropping the oldest insertion;
  - cleared for an agent when its connection generation changes;
  - an entry is removed when a view takes it.
- **Drained when a view for the key is created.** This happens in `launch` (`store.rs:2919`) and `open_session` (`:2999`). The entry is passed to `view.queue_session_update` before any other input, so a later load or update applies on top of it. In `open_session` the take happens only after the checks that can still return (agent known), so an early return cannot consume the entry.

*Alternatives:*
- **Buffer every `session/update` while a creation is in flight.** Rejected: replaying transcript or config updates before a load could duplicate content or undo options set during launch.
- **Create the view right after `session/new` returns.** Rejected: launch deliberately applies mode/model/reasoning before showing the session and sending the first prompt. Reordering it is a larger change, and it overlaps `add-quick-session-panel`'s rework of `launch`.

When `add-quick-session-panel` lands, `early_commands` becomes part of its `SessionKey` routing: the first surface that opens the key drains it.

### D4. gpui reads commands from the projection
- `map_header` copies `available_commands` into `HeaderFields`.
- `refresh_header` sets `SessionView.slash_commands` (always, including to empty: an empty update clears).
- A helper `Store::session_slash_commands() -> &[SlashCommandView]` returns the open session's list or an empty slice.

The palette and the composer list read only this helper.

### D5. Palette group and targets built together
- **Building:** `palette_view` builds the "Slash commands" group and its targets in the same step. The group is placed after Actions and before Go to.
- **Only when useful:** it is added only when `page == Page::Session` and the list is non-empty.
- **Keeping indexes aligned:** to stop `targets` drifting from groups (the Sessions group is conditional too), `palette_view` collects `(CommandGroup, Vec<PaletteTarget>)` pairs in one `Vec`, then splits them. Indexes cannot diverge.
- **Filtering (decided while implementing).** gpui-kit's `Command` matcher tests the whole query as one substring of the label or of a single keyword, so the spec's `free context` could not find "…to free up context". The palette therefore turns the kit's filter off (`filterable(false)`) and filters itself with `search::rank` (D8) over each entry's label and keywords, within each group (group order is fixed, empty groups are dropped).
  - **Groups follow their best match (added after review).** With the fixed group order, `comp` listed the action `Schedule a prompt…` (its letters are scattered over the label) above the command `compact`, and Enter ran the weaker match. `filter_palette` now keeps each group's best score and sorts the groups by it (stable: equal scores keep Actions, Slash commands, Go to, Sessions). The first row is the best match of the whole palette. `Schedule a prompt…` stays listed, below. An empty query keeps the built order.
  - Within a group the label is the primary field and the keywords (description, hint, `automation`, …) are secondary (D8).
  - `palette_view` reads the query from the palette state at render; `on_query` only notifies the app, which rebuilds the groups.
  - `on_confirm`'s index path counts into the groups as built for that query, so targets still match rows.
  - The existing groups (Actions, Go to, Sessions) use the same filter, so their search becomes word-based and typo-tolerant too.
- **Entries:**
  - label: the bare `name`, without `/`. The palette lists commands; it does not complete text, so the prefix adds noise;
  - a **second line** with the agent's description (`PaletteEntry.description`), dimmed and cut off with an ellipsis; an empty description shows nothing;
  - description and hint also stay as keywords, so the search matches them;
  - icon `SquareSlash` (also added to the web build's icon list in `main.rs`).
- **Two-line rows.** gpui-kit draws only an icon and a label unless an item has custom content (`CommandItem::child`), and custom content replaces the icon and label entirely. `palette_view` therefore draws such a row itself for entries with a description (icon, name in the text colour, description in the muted colour below it) and keeps the kit's own row for the others. The label and keywords are still set on the item, so the app's search and `on_confirm` index paths are unaffected. The kit measures custom rows again on every render (`same_layout` is false for them); the number of commands is small, so this is fine.
- **New target:** `PaletteTarget::SlashCommand { name, takes_input }`.
  - **Without input:** `run_palette` calls `store.send_prompt(format!("/{name}"), vec![], self.delivery, cx)`. It uses the same readiness check as `send_session_prompt` and returns without sending when not ready. While a turn runs, `send_prompt` already queues slash commands.
  - **With input:** the target sets the composer to `/name `, moves the cursor to the end and focuses it. The composer list (D6) then shows the hint.

### D6. Composer command list
State on `QmtApp`:
- `slash_cursor: usize`;
- `slash_dismissed: Option<String>`, the composer text at which Esc closed the list.

**When the list is shown.** It is derived on render, not stored:
- the composer text starts with `/`;
- the text before the cursor contains no whitespace;
- the session has commands;
- `slash_dismissed != Some(current text)`;
- the composer has focus (added while implementing: without it a list left behind by a click elsewhere would stay on screen, and its Esc would shadow the Esc of an overlay that took the focus).

The query is the text between `/` and the cursor.

**Matching.** Uses D8 against the command **name only**. Matching the description makes short queries match unrelated commands; for example, `wt` matches `/compact`'s "now to". The palette is the place to search descriptions.

**Keys.** Handled by `capture_action` handlers on the composer wrapper, placed before the existing ones. Each acts only while the list is shown, and otherwise lets the action through:
- `MoveUp` / `MoveDown` move the cursor.
- `Enter` (only when the send shortcut would submit) and `IndentInline` (Tab) complete. Completion replaces the first word with `/name ` and keeps the rest of the text. The textarea cursor goes after the space.
- Escape is not a textarea action here. When the list is shown, Esc sets `slash_dismissed` and counts as handled, which resets `last_escape` and stops the key (the root Esc handler's existing true-branch). Implemented as `close_slash_list(window, cx)`, the first test of that handler's chain, ahead of `close_topmost()`: `close_topmost()` has no `cx` or `window` and other callers (`agents.rs`, tests), and the list is the innermost thing, so it closes first.

**Rendering.** A `deferred` popover above the composer, like `session_options_panel`, with at most 8 visible rows and a scroll handle. Each row shows:
- `/name` in mono;
- the description, truncated;
- the dimmed hint.

Clicking a row completes it.

**Hint after completion.** When the text is exactly `/name ` for a command with a hint, a one-line hint strip is shown above the textarea: `/name` and the hint, dimmed. The `Textarea` placeholder only shows for empty text, so it cannot show the hint after `/name `.

### D9. A background load continues on the view's own core
`begin_load` flushes the queue, then **moves the view's core into the `LoadJob`** (`std::mem::replace` with an empty `SessionCore::new(session_id)` as a placeholder). `hydrate` applies `SnapshotLoaded` to that core, and `finish_load` hands it back through `replace_core`. A background load now behaves like `load`: whatever the header held before the response survives, because `hydrate_header` leaves `available_commands` alone (D1).

- **The placeholder is never fed.** `flush` returns early while `hydrating`, so inputs that arrive during the decode queue behind it, as before, and are applied after `finish_load`. Nothing reads the placeholder in between: the UI reads the view's copied header fields, which keep their values until `finish_load` refreshes them.
- **Same precondition.** `begin_load` still falls back to `load` when the view already has turns, so the core moved out never holds a transcript.
- **Why not only the commands:** copying `available_commands` into the job would fix this symptom but leave the next header-only update (config, title, mode) to be dropped the same way; the early commands of D3 would also depend on it. Moving the core fixes the class.
- **Rejected: re-sending the list after `finish_load`.** It depends on the order of arrival and on a QueryMT that never repeats the advertisement.
- **Tests:** `background_load_keeps_the_commands_advertised_before_it` and `background_load_applies_a_later_advertisement_on_top` (`view_tests.rs`); `early_commands_survive_a_background_load` (`early_commands.rs`) for both an advertisement made while the load is in flight and one made before the view existed. Checked by mutation: with `hydrate` building a fresh core again, the first and the last fail.

### D7. Model picker cursor
- **New state on `QmtApp`:** `model_cursor: usize` and `model_scroll: ScrollHandle`.
- **One source for the rows.** A new `model_entries(cx) -> Vec<PickerEntry>` returns what `model_rows` renders, in order: `Heading`, `Model(Model)` or `More(n)`. It already applies `GROUP_ROW_LIMIT`. `model_rows` renders from it. The selectable list is the `Model` entries, and each entry's index in the vector is its scroll index.
- **Keys.** `model_search` wraps its field in `capture_action(MoveUp/MoveDown)`, which moves the cursor (wrapping), calls `scroll_to_item` and stops propagation. The `PressEnter` subscription calls `pick_highlighted_model`.
- **Scrolling.** The modal, delegate and setup lists use `.track_scroll(&self.model_scroll)`. Only one picker is visible at a time.
- **One pick path.** `pick_model(key)` routes the delegate when one is being edited, else calls `select_model`. Both row clicks and Enter call it. `pick_first_model` is removed.
- **Cursor resets.**
  - `open_model_picker` and the delegate's `pick_delegate` set the cursor to the selected model's index, else 0.
  - A `model_query` `Change` event sets it to 0. It is clamped on render.
- **Highlight style.** `card_hover` background, distinct from the selected row's style and from mouse hover. Hover does not move the cursor, as in `profile_select`.

### D8. One matcher on frizbee
`src/search.rs` (new) wraps `frizbee` 0.13:

```rust
pub fn rank<'a, T>(query: &str, items: &'a [T], fields: impl Fn(&T) -> String) -> Vec<(usize, u16)>
```

It returns `(item index, score)` for matches, best first; equal scores keep the original order.

- **Normalization.** Queries and haystacks are lowercased, and `-` `_` `.` `/` `:` become spaces.
- **Field tiers (added after review).** `rank_tiered(query, items, primary, secondary)`; `rank` is the case with no secondary fields.
  - A word's score for an item is the best of its primary fields' scores and **half** of its best secondary field's score. The item's score is still the sum over words.
  - Secondary fields describe or locate an item rather than name it. Without the tier, `op` begins the provider `openrouter`: every model of that provider scored as well as a name match, tied, and led the list in list order, above `claude-opus-*`; the same provider match also stopped a model's own better name from ranking it first within the provider's group.
  - Halving, not a filter: a word that matches only a secondary field still matches (`openrouter` lists that provider's models).
  - Per caller, primary / secondary:
    - model picker: label, bare model name / id (which repeats the provider as a prefix), provider, node;
    - profile and target select: name / id, description;
    - workspace field: workspace name / path;
    - command palette: label / keywords;
    - composer slash list and folder names: one tier.
  - The model name, not the id, is primary because `provider/model` ids begin with the provider.
- **Words.**
  - Each query word becomes a `Pattern::new(word, PatternConfig::default())`.
  - `max_typos` is `len / 4` for words of 5 or more characters and 0 otherwise.
  - `Matcher::from_patterns` requires every pattern to match.
  - `Pattern::new` is used instead of `Matcher::from_query`, so `!`, `^`, `'` and `$` stay literal.
- **Haystack.** One per field (revised in task 1.2): each word is matched against each field separately and takes the best field score, so a word never spans two fields.
- **Empty query.** Returns every index with score 0.

Callers:
- **Model picker:** fields label, id, provider, node. Groups are ordered by their best score, and models within a group by score.
- **Profile and target select:** name, id, description.
- **Workspace field:** workspace name, then path.
- **Command palette:** label and keywords (the slash entries' description and hint; sessions' workspace name).
- **Composer slash list:** name only.

`util::fuzzy_score`, `word_score` and their test are removed. The existing behavioural tests in `profile_select.rs` and `workspace_select.rs` stay and must pass unchanged, except for the "earlier fields rank higher" assertion, which is dropped.

*Alternatives:*
- **`nucleo-matcher`.** Similar quality, but frizbee is faster and has typo tolerance. The user asked for frizbee.
- **Weighting fields by running one matcher per field.** Rejected at first (no requirement, more work). Adopted, as two tiers, after review: since task 1.2 `rank` already matches per field, so the tiers cost one comparison, and the ordering reports (`op`, `comp`) needed them.
- **Requiring a word to match inside one word of a field (so `comp` would not list `Schedule a prompt…`).** Rejected: it breaks `cdl` → "Coder Delegate". The weaker match stays listed, ranked below.

## Risks / Trade-offs

- **[Golden traces churn in QueryMT: every header gains `availableCommands`]** → Regenerate in a dedicated commit that contains only the trace diff. gpui's differential test reads `golden_cases()`, so it needs no fixture update.
- **[Custom palette rows are taller than the kit's, so fewer entries fit in the list's 380 px]** → Only slash entries with a description are two lines; the list scrolls.
- **[A background load leaves an empty placeholder core in the projection while it decodes]** → Nothing reads it (D9); `finish_load` replaces it before any input is applied.
- **[Bare command names change what QueryMT's other clients see]** → No consumer in the QueryMT repo reads `availableCommands`. External ACP clients that follow the spec already add the `/`; the core strips it either way.
- **[In the palette, typo tolerance lets a long description match a longer query loosely, e.g. `worktree` also lists `/compact`, below `/worktree`]** → Ranked best first; the composer list matches names only.
- **[Halving is a heuristic: a match only in a secondary field can still outrank a weak name match]** → The factor is a single constant in `search.rs`; the tests pin `op` (model picker and matcher) and `comp` (palette).
- **[Ordering the palette's groups by score moves the groups around while typing]** → Only while there is a query; equal scores keep the built order, and the first row is always the best match, which Enter runs.
- **[Typo tolerance lets short queries over-match long model ids]** → Typos only for words of 5+ characters, and results are ranked. Tests pin `gpt5`, `sonnet 4.5`, `anthropic sonnet`, `cl son` and `sonet`.
- **[Separator normalization splits `4.5` into the words `4` and `5`, and single digits match widely]** → Ranking puts contiguous matches first. Acceptable for picker sizes in the hundreds.
- **[frizbee behaviour on wasm (SIMD backend selection)]** → The task verifies the wasm build and runs the search tests natively. frizbee falls back to scalar code when SIMD is unavailable.
- **[Composer Enter now completes instead of sending while the list is open]** → The list only shows while the first word is being typed and matches a command. Esc closes it, and Enter then sends.
- **[Pin bump also moves `use-agent-session-core`'s pinned rev]** → Record the new sha in both changes' tasks and refresh `flake.nix` `outputHashes` with the same procedure as that change's task 2.1.

## Migration Plan

1. QueryMT commits on `feat/agent-session-core`, pushed: D1 (core + golden case + regenerated traces), then D2 (bare names).
2. gpui moves to the pushed sha: both `Cargo.toml` tables, `Cargo.lock`, and `flake.nix` `outputHashes`.
3. gpui: D8 matcher, D7 picker, D3/D4 commands, D5 palette, D6 composer list. Each step builds and passes tests on its own.

Rollback: revert gpui to the previous pin. Without the header field, gpui does not compile against the old core, so the gpui commits of steps 3 (D3–D6) are reverted together with the pin. D7 and D8 do not depend on the pin.
