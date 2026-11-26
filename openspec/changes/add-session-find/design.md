# Design

## Context

- **Transcript:**
  - The transcript is a virtualized `gpui::list` (`Transcript.list`, `src/ui/session_view.rs:244`) of `Row`s (`:3464`). A row is a whole `Item` or, for a large expanded work group, its header or one block.
  - Rows outside the viewport are not built, so their elements do not exist.
- **Markdown views:**
  - Markdown in rows goes through `md::Kept` (`src/ui/md.rs:150`). It keeps one `Entity<TextViewState>` per view id and records which view ids each row built (`begin_row` / `end_row`, called around `render_row` at `session_view.rs:5729`).
  - The tier 1 view ids are:
    - User messages: `umd-{key}`, built from the text after `split_front_matter`, trimmed (`:3313`).
    - Replies: `md-{key}` (`:3424`).
  - The tier 2 view ids are:
    - Reasoning: `th-md-{thought.id}` (`:2813`, `:2825`).
    - Skill bodies: `skill-md-{call.id}`.
    - Delegation briefs: `delegate-objective-…` and `delegate-detail-…`.
    - Tool results shown as Markdown (`:2285`).
- **Range highlights in gpui-base 0.11 (`TextViewState`):**
  - `rendered_text()` is the plain-copy text, with blocks separated by `\n`.
  - `set_range_highlights()` paints backgrounds over byte ranges of that text without changing layout. The highlights follow appended text while streaming.
  - `reveal_range()` scrolls a range's line into view, through the enclosing `gpui::list`, once the row is laid out.
- **Code output:** tool output shown as code goes through `code::HighlightedLines::line` (`src/ui/code.rs:98`). That builds a `StyledText` with `HighlightStyle` runs, which can also carry a background.
- **Keys:**
  - App bindings have no key context and are handled on `QmtApp`'s root (`src/ui/keys.rs`).
  - gpui-base binds `cmd-f` / `ctrl-f` to `input::Search` in the `Input` context. The handler calls `cx.propagate()` when the input is not `searchable` (`gpui-base/src/input/editor/search.rs:302`), and GPUI then tries the next matching binding (`gpui-pre/src/window.rs:6682`). The composer is not searchable, so an app-level `cmd-f` binding is reached from the composer.
- **Expansion:** expanded sections are keys in `SessionPane.expanded`. They feed row signatures (`RenderCtx::sig`), so changing them re-measures only the affected rows.
- **Quick panel:** the quick panel hosts its own `SessionPane` in another window. Its root does not handle the new action, so the shortcut does nothing there.

## Goals / Non-Goals

**Goals:**
- Tier 1 ships alone. Tier 2 adds sources and auto-expansion without reworking tier 1's model.
- The count is exact for what is searched, including rows never built.
- No per-frame cost when find is closed. With find open, editing the query or streaming costs time proportional to the items that changed, not to the whole transcript.

**Non-Goals:**
- Highlighting inside custom Markdown blocks (tables wrapped as `qmt-table`, and Mermaid diagrams once `render-mermaid-diagrams` lands). gpui-base leaves custom blocks unpainted. Their text is still counted, and the reveal scrolls to the block.
- Paging in older history to search it.

## Decisions

### D1. Two layers: a data index for counting and navigation, and painting on built views

- **Index (data):**
  - `FindIndex` lives in a new `src/ui/find.rs` and is owned by `SessionPane`. It builds the ordered match list from the `SessionView` items without any elements.
  - Each match is a `FindMatch { item_key, view_id, occurrence }`, where `occurrence` is the k-th match in that view's text.
  - The index orders matches by item index, then by view order within the item, then by occurrence.
- **Paint (elements):**
  - `md::Kept` holds the active `FindQuery`: the query, the current match's `(view_id, occurrence)`, and the two highlight colours.
  - When `Kept` resolves a view's state for rendering (`Kept::state`), it searches that view's `rendered_text()` with the same matcher, sets range highlights, and gives the current occurrence the strong colour.
  - Each `KeptView` stores a stamp (find generation, rendered revision) so highlights are recomputed only when the query, the current match or the text changed. This avoids notify loops during render.
- **Why:** off-screen rows have no `TextViewState`, so counting has to come from data. Painting has to use `rendered_text()`, because those are the offsets `set_range_highlights` takes.
- **Alternatives considered:**
  - Creating headless `TextViewState`s for every item to search. This costs one entity and one parse per item, and large parses run asynchronously.
  - Searching only built rows. The count would be wrong, and Enter could not reach off-screen matches.

### D2. One matcher and one source table, shared by both layers

- **Matcher:**
  - `find::match_ranges(text, query) -> Vec<Range<usize>>` matches case-insensitively, without overlaps.
  - A whitespace run in the query matches any run of spaces or tabs in the text. It does not match `\n`, so a match never crosses a block boundary.
  - Case folding uses `char::to_lowercase` per char, with a byte map back into the original string, so non-ASCII offsets stay on char boundaries.
- **Searchable text for the index:**
  - `find::searchable_text(source)` reproduces `rendered_text()` from the Markdown source: the mdast blocks are joined with `\n`, and inline syntax is stripped as `md::plain_text` does, but newlines are kept.
  - A test runs a fixture corpus through both functions and requires equal match counts: emphasis, links, inline code, lists, headings, code fences, tables, soft breaks and CJK.
- **Source table:**
  - `find::views_of(item, developer) -> Vec<(view_id, source)>` lists the views an item shows and the exact text each one gets.
  - The row renderers call the same helpers for their ids and text (`umd-`/`md-` in tier 1), so search cannot drift from rendering.
- **Fallback:** if the painted occurrence k does not exist in the rendered text, for example because of an unforeseen difference, the paint layer highlights the view's matches without a current one and reveals the view's first match. The count shown is still the index's.

### D3. Incremental index

- **Caching:** `FindIndex` caches each item's `(rev, Vec<(view_id, match count)>)`.
  - On a query change, it re-runs the matcher over the cached searchable texts. Those texts are cached per `(item key, rev)` while find is open, and dropped on close.
  - On a session notify, only items whose `rev` changed, or which are new, are re-read.
- **Ordering:** `FindIndex` keeps a vector of item keys in transcript order, rebuilt only when items are added or prepended. That is the same key list `Transcript` already compares in `keys`.
- **Current match identity:**
  - The current match is held as a `FindMatch`. After a recompute it is located again by `(item_key, view_id, occurrence)`.
  - If that match no longer exists, the current match becomes the next one in order. If there is none, it becomes the last match.
  - Appending text only adds matches after the current one in its view, so the current match is preserved.

### D4. Navigation: scroll the row, then reveal the range

- **Moving to a match:**
  1. Map `item_key` to its row index. For split work groups (tier 2), use the row whose block contains the view.
  2. Call `transcript.list.scroll_to_reveal_item(row_ix)` and pause following the tail (`pause_following_tail`).
  3. Set `Kept`'s current match. When the row is next built, `Kept` sets the highlights and calls `reveal_range(range)` on that view, so the exact line comes into view inside a tall reply.
- **First match after a query change:** the first match whose item index is at or after the item of `list.logical_scroll_top().item_ix`. If there is none, the first match overall.
- **Why:** `reveal_range` needs the row laid out, and only `scroll_to_reveal_item` can bring an unbuilt row in. The documented pattern in gpui-base is exactly "scroll to that row first".

### D5. Find bar UI and keys

- **Action:** `FindInSession` is declared in `keys.rs` and bound to `cmd-f` on macOS and `ctrl-f` elsewhere, with no context. `QmtApp` handles it only when `page == Page::Session` and a session is shown, by calling `pane.open_find(window, cx)`.
- **Bar placement and contents:**
  - The bar floats at the top right of the transcript area, inside the `relative()` container that holds the list (`session_view.rs:5716`). It sits above the top fade, follows the existing card and popover styling in `kit`, and does not change the list's layout.
  - It contains:
    - a single-line `InputState`
    - the `n of m` / `No results` label
    - up and down icon buttons
    - a close button.
- **Keys in the bar:**
  - `InputEvent::PressEnter { shift }` handles Enter (next) and Shift+Enter (previous).
  - `InputEvent::Change` re-queries. It runs immediately for queries up to 10k items. The index is synchronous; a background pass is a later option, see Risks.
  - The bar's own `capture_action(Escape)` closes it and stops propagation, as `workspace_select.rs:498` does. The app's double-Escape tracker therefore never sees that press.
  - Cmd+F while the bar is open focuses the input and selects all.
- **Closing:** `close_find` clears `Kept`'s query, which removes every highlight with `clear_range_highlights` on each kept view that has one, drops the caches and restores focus via `restore_focus`.
  - `SessionPane::show` and `clear` (session switch) close find.
- **Colours:** the other matches use the accent at low alpha (the `accent_dim` family). The current match uses the accent at higher alpha, so the text stays readable in both themes.

### D6. Tier 2: more sources, code highlights and auto-expansion

- **Sources:** `views_of` adds:
  - reasoning (`th-md-{id}`)
  - skill and delegation Markdown
  - tool results shown as Markdown
  - tool output shown as code, as pseudo-views `code-{block key}` whose searchable text is the raw output the code card shows (tabs expanded as in `HighlightedLines::line`).

  Notices and compaction summaries stay out.
- **Code highlights:**
  - `HighlightedLines::line` takes an optional `&[(Range<usize>, bool /*current*/)]` of matches on that line and adds background `HighlightStyle` runs, merged with the syntax runs. GPUI's `StyledText` combines overlapping highlight styles.
  - Revealing a code match scrolls its row in. Inside fixed-height tool blocks, the code block's own `ScrollHandle` (`RenderCtx::block_scroll`) scrolls to the line.
- **Auto-expansion:**
  - Moving to a match whose view is hidden inserts the needed keys into `SessionPane.expanded` before scrolling:
    - the work group's `item.key`
    - the reasoning `thought.id`
    - the tool call's `call.id` or `tool_details_key`.
  - `find::reveal_keys(item, view_id)` returns that list, beside `views_of`.
  - Expansion changes row signatures, so only that item's rows are re-measured. The scroll then uses the new row index of the target block.
- **After close:** expansions are left as they are (spec: expansion kept after closing).

## Risks / Trade-offs

- [The rendered text and the reconstructed searchable text disagree for some Markdown] → The equal-count corpus test (D2) catches known cases, and the fallback paints without a wrong current match. The count still comes from the index, so the shown number can differ from the visible highlights only in rare cases.
- [Synchronous re-query on very large sessions (thousands of long replies) stalls typing] → Searchable texts are cached, so a query change is a substring scan only. If profiling shows more than 8 ms at 2,000 items, move the scan to `cx.background_spawn` and keep the last result until the new one lands. Tasks include a timing test.
- [`reveal_range` drops a request that cannot be shown within a second] → Navigation re-issues the reveal on the next build of the target row while it is still current.
- [Setting highlights inside render notifies the `TextViewState`] → The stamp (D1) makes it idempotent, and the notify affects only that view's entity, not the pane.
- [A table or Mermaid block's text is counted but not painted] → This is accepted. The reveal still brings the block into view. It is documented under non-goals.
- [Auto-expansion (tier 2) can move content above the viewport] → The list's scroll anchoring keeps the anchor row in place (the existing expansion test at `session_view.rs:6244`), and the scroll goes to the target afterwards.

## Migration Plan

None. This is a new UI feature with no stored state. To roll it back, remove the binding.
