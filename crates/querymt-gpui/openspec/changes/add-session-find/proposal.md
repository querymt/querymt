# Proposal

## Why

The only way to find something said earlier in a session is to scroll. Long sessions run to thousands of transcript rows, and the transcript is virtualized, so off-screen rows are never built and the platform's own find never sees them. Users need an editor-style "find in page" for the open session.

## What Changes

The change lands in two tiers. Tier 1 is complete on its own. Tier 2 extends it to text that sits behind disclosures.

**Tier 1: messages and replies (cheap)**

- **Cmd+F (Ctrl+F on Linux and Windows) opens a find bar** over the main window's transcript. The shortcut works whether the composer, the transcript or nothing in particular is focused. Pressing it again while the bar is open focuses the bar and selects its query.
- **Searched text:** the user's messages and the agent's replies of the loaded transcript. Matching is a literal, case-insensitive substring match on the text as rendered, so Markdown syntax is not matched.
- **Results:** the bar shows "n of m". Enter and Shift+Enter (and the bar's arrow buttons) move to the next and previous match, wrapping at the ends. The current match scrolls into view and is highlighted more strongly than the other matches in view.
- **Live:** the count updates as replies stream. The current match stays the current match while text is appended.
- **Closing:** Escape (or the bar's close button) closes the bar, clears every highlight and returns focus to the composer. Following the tail of the transcript is not resumed by closing.
- **Out of tier 1:** reasoning, tool calls and tool output, notices, compaction summaries, and history not yet loaded.

**Tier 2: reasoning and tools (follow-up tasks in the same change)**

- Reasoning blocks, tool-call content rendered as Markdown (skill bodies, delegation briefs, tool results) and tool output shown as code (read results, shell output) become searchable.
- Moving to a match inside a collapsed work group, reasoning block or tool call expands it. Closing the bar keeps what it expanded, so the reading position stays where it is.

**Not in this change**

- The `/` shortcut. It conflicts with slash commands in the composer.
- Find in the quick panel.
- Searching history that has not been loaded yet. Find covers what the transcript holds. It does not page older turns in.
- Regex, whole-word and case-sensitive options.

## Capabilities

### New Capabilities

- `session-find`: finding text in the open session's transcript: how find opens and closes, what text it searches, how matches are counted, navigated and highlighted, and how it behaves while the session streams.

### Modified Capabilities

None. `picker-search` covers fuzzy ranking in pickers. Session find is a literal match and does not change it.

## Impact

- `src/ui/keys.rs`: a `FindInSession` action bound to `cmd-f` / `ctrl-f` with no key context. The composer's `Input` binding for `cmd-f` propagates when the input is not searchable, so the app binding still fires.
- `src/ui/session_pane.rs` / a new `src/ui/find.rs`: find state (query input, match list, current match), the bar, and its keys.
- `src/ui/session_view.rs`: highlights handed to the rendered rows. Escape handling in `src/ui/mod.rs` closes the bar before the double-Escape cancel.
- `src/ui/md.rs`: `Kept` applies range highlights to the text views of a row (`TextViewState::set_range_highlights` and `reveal_range` from gpui-base 0.11).
- Tier 2 only: `src/ui/code.rs` draws match backgrounds in code lines, and auto-expansion goes through `SessionPane::expanded`.
- No new dependencies. No protocol or agent changes.
