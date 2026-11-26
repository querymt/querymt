# Tasks

Verification commands:
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`

Tier 1 is groups 1 to 4 and ships on its own. Tier 2 is groups 5 to 7. If tier 2 is deferred, move its two requirements ("Find covers reasoning and tool content" and "Moving to a hidden match reveals it") out of `specs/session-find/spec.md` into a follow-up change before archiving.

## 1. Tier 1: matcher and index (no UI)

- [x] 1.1 Add `src/ui/find.rs` with `match_ranges(text, query)` (design D2): case-insensitive, non-overlapping, a query whitespace run matches spaces and tabs but not `\n`, and offsets stay on char boundaries. Verify: unit tests for case, repeated and overlapping patterns (`aa` in `aaa` gives 1), whitespace runs, a newline boundary, and non-ASCII (`Straße`, CJK).
- [x] 1.2 Add `searchable_text(source)`, which mirrors `TextViewState::rendered_text()` (D2). Verify: a `#[gpui_kit::test]` corpus test renders each fixture through `TextViewState::markdown` and asserts equal `match_ranges` counts against `rendered_text()`. Fixtures: emphasis, links, inline code, lists, headings, a code fence, a table, soft breaks, CJK.
- [x] 1.3 Add `views_of(item)` for tier 1 (user messages with front matter split off and trimmed, and replies) and route the `umd-` and `md-` id and text construction in `session_view.rs` through the same helpers. Verify: a unit test that `views_of` ids and texts equal what `render_item` passes to `Kept::markdown` for a user message with front matter and for a reply; the existing transcript tests pass.
- [x] 1.4 Implement `FindIndex` (D1, D3): an ordered `FindMatch` list, per-item `(rev, counts)` caching, current-match identity across recomputes, and next/previous with wrap. Verify: unit tests for spec scenarios "Count shown", "Nothing found", "Next wraps around", "Previous", and "Streaming reply adds matches" (append to the current item and the current match keeps its identity), and that a recompute after one item's `rev` changes re-reads only that item.

## 2. Tier 1: painting matches in built views

- [x] 2.1 Extend `md::Kept` with the active `FindQuery` and the per-view stamp. In `Kept::state`, set range highlights (other matches in the weak colour, the current match in the strong colour) and `reveal_range` for the current match. Clear them all when the query is cleared (D1, D5 colours). Verify: a `#[gpui_kit::test]` in `md.rs` that builds a kept view, sets a query, and asserts the highlight ranges on the `TextViewState`; that rebuilding with the same stamp does not notify again; and that clearing removes the highlights.
- [x] 2.2 Add the fallback for a missing occurrence (D2): highlight without a current match and reveal the first match. Verify: a unit test with a deliberately mismatched occurrence index.

## 3. Tier 1: find bar, keys and navigation

- [x] 3.1 Declare `FindInSession` in `keys.rs`, bind it to `cmd-f` / `ctrl-f` with no context, and handle it on `QmtApp` only for `Page::Session` with a session shown (D5). Verify: GPUI tests for "Open from the composer" (the composer focused, the bar opens, the draft is unchanged), "Open again while open" (the query is selected), and "No session open" (nothing opens). Update the shortcut list in the module docs of `keys.rs`.
- [x] 3.2 Render the find bar over the transcript (input, `n of m` / `No results`, up, down, close) and wire `Change`, `PressEnter { shift }` and the buttons to `FindIndex` (D5). Verify: GPUI tests that typing updates the label, and that Enter, Shift+Enter and the buttons step through the matches.
- [x] 3.3 Navigation (D4): `scroll_to_reveal_item` for the current match's row, pause following the tail, set `Kept`'s current match, and pick the first match at or below the viewport top on a query change. Verify: GPUI tests for "Match far off screen" (a match about 500 rows up becomes visible and following the tail is paused), "Typing picks a nearby match", and "Highlight does not move content" (row bounds are unchanged after the query is set).
- [x] 3.4 Closing (D5): the bar's capture of Escape, the close button, `SessionPane::show` and `clear` close find, clear highlights and restore focus. Verify: GPUI tests for "Escape closes", "Escape during a running turn" (two Escapes within 700 ms close the bar and do not cancel), and "Switching sessions".
- [x] 3.5 Keep find live while streaming: recompute on the session's notify only while open. Verify: a GPUI test that streams chunks into the last reply with the bar open and asserts the label and an unchanged scroll offset (spec: "Streaming reply adds matches").

## 4. Tier 1: integration checks and docs

- [x] 4.1 Add a "Find in session" section to `knowledge/ui.md` covering the two layers, the shared matcher, `Kept` painting and key routing. Verify: the section names the files and the design decisions, and `cargo fmt` / docs build are clean.
- [x] 4.2 Run the verification commands and `openspec validate add-session-find --type change --strict --no-interactive`. Verify: all pass; record the test counts here.
  Recorded: `cargo nextest run` 579 passed, 5 skipped (the skipped ones are the existing `#[ignore]` timing tests); clippy `-D warnings`, `cargo fmt --check`, the wasm32 build and `openspec validate add-session-find --type change --strict --no-interactive` pass.
- [x] 4.3 By hand on macOS with a long session (1,000+ items) while a turn streams:
  - Cmd+F from the composer
  - stepping across off-screen matches
  - typing latency
  - Escape behaviour
  - the quick panel ignoring Cmd+F.


## 5. Tier 2: reasoning and Markdown tool content

- [ ] 5.1 Extend `views_of` with reasoning (`th-md-`), skill bodies, delegation briefs and tool results shown as Markdown, routing those renderers' ids and text through the shared helpers (D6). Verify: unit tests that `views_of` matches the renderers' ids and texts, and that notices and compaction summaries are excluded; the spec scenario "Match in reasoning" passes as a `FindIndex` test.
- [ ] 5.2 Add `reveal_keys(item, view_id)` and apply them to `SessionPane.expanded` before navigating (D6). Verify: GPUI tests for "Match in a collapsed work group" (the group and reasoning expand and the match is visible and current) and "Expansion kept after closing".

## 6. Tier 2: tool output shown as code

- [ ] 6.1 Add code pseudo-views (`code-{block key}`) to `views_of`, with searchable text equal to the code card's text (tabs expanded). Verify: a unit test on a read result and on shell output.
- [ ] 6.2 Pass match ranges into `HighlightedLines::line` and merge background runs with the syntax runs; scroll fixed-height tool blocks to the current line through their `ScrollHandle` (D6). Verify: a unit test of the merged runs (a match spanning a syntax-run boundary, tab-expanded offsets) and a GPUI test for "Match in tool output" (counted, highlighted, and the line is visible inside a clamped block).

## 7. Tier 2: integration checks and docs

- [ ] 7.1 Update the "Find in session" section of `knowledge/ui.md` with the tier 2 sources, code highlights and auto-expansion. Verify: the section reflects D6.
- [ ] 7.2 Run the verification commands, a timing test (2,000 items with reasoning and tool output, a query change under 8 ms in release or a recorded decision to move the scan to the background per the design's Risks), and `openspec validate add-session-find --type change --strict --no-interactive`. Verify: all pass; record the numbers here.

## Workflow follow-up

- Archive after both tiers are done, or after moving the tier 2 requirements to a follow-up change as described at the top.
