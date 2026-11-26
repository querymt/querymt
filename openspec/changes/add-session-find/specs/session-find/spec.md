# Spec Delta

## Purpose

Lets the user find text in the open session's transcript, like an editor's find-in-page: open a find bar, see how many matches there are, step through them and see each one highlighted in place.

## ADDED Requirements

### Requirement: Open find with the shortcut
In the main window, while a session is open, Cmd+F on macOS and Ctrl+F on Linux and Windows SHALL open a find bar above the transcript and focus its query field. The shortcut SHALL work whether the composer, the transcript or nothing is focused. When the bar is already open, the shortcut SHALL focus the query field and select its text.

#### Scenario: Open from the composer
- **GIVEN** a session is open and the composer has focus
- **WHEN** the user presses Cmd+F
- **THEN** the find bar opens with an empty query field that has focus, and the composer's text is unchanged

#### Scenario: Open again while open
- **GIVEN** the find bar is open with the query `cache` and the transcript has focus
- **WHEN** the user presses Cmd+F
- **THEN** the query field has focus and `cache` is selected

#### Scenario: No session open
- **GIVEN** the Start page is shown
- **WHEN** the user presses Cmd+F
- **THEN** no find bar opens

### Requirement: Find covers messages and replies
Find SHALL search the user's messages and the agent's replies held by the transcript. It SHALL match the text as displayed, without Markdown syntax, as a literal case-insensitive substring. Text not yet loaded from older history SHALL NOT be searched.

#### Scenario: Match across Markdown formatting
- **GIVEN** a reply whose source is `the **retry** loop`
- **WHEN** the user searches for `retry loop`
- **THEN** that reply has one match

#### Scenario: Case is ignored
- **GIVEN** a user message `Fix the Parser`
- **WHEN** the user searches for `parser`
- **THEN** that message has one match

#### Scenario: Markdown syntax is not matched
- **GIVEN** a reply whose source is `the **retry** loop`
- **WHEN** the user searches for `**`
- **THEN** that reply has no match

### Requirement: Match count
The find bar SHALL show the position of the current match and the total, as `n of m`. With a non-empty query and no matches it SHALL show `No results`. With an empty query it SHALL show neither, and no text SHALL be highlighted.

#### Scenario: Count shown
- **GIVEN** the transcript contains `token` three times
- **WHEN** the user types `token`
- **THEN** the bar shows `1 of 3`

#### Scenario: Nothing found
- **WHEN** the user types a query that occurs nowhere in the searched text
- **THEN** the bar shows `No results` and the transcript does not scroll

### Requirement: First match follows the reading position
When the query changes, the current match SHALL be the first match at or below the top of the transcript's viewport. If there is none, it SHALL be the first match in the transcript.

#### Scenario: Typing picks a nearby match
- **GIVEN** the transcript is scrolled to its middle and `error` occurs both above and below the viewport's top
- **WHEN** the user types `error`
- **THEN** the current match is the first occurrence at or below the viewport's top

### Requirement: Step through matches
Enter and the bar's down button SHALL move to the next match, and Shift+Enter and the up button to the previous one, in transcript order. Moving past the last match SHALL go to the first, and moving before the first SHALL go to the last. The transcript SHALL scroll so the current match is visible.

#### Scenario: Next wraps around
- **GIVEN** the bar shows `3 of 3`
- **WHEN** the user presses Enter
- **THEN** the bar shows `1 of 3` and the first match is visible

#### Scenario: Previous
- **GIVEN** the bar shows `2 of 3`
- **WHEN** the user presses Shift+Enter
- **THEN** the bar shows `1 of 3` and that match is visible

#### Scenario: Match far off screen
- **GIVEN** the next match is in a message hundreds of rows above the viewport
- **WHEN** the user presses Enter
- **THEN** the transcript scrolls to that message, the match is visible, and following the tail is paused

### Requirement: Matches are highlighted
Every match in view SHALL get a background highlight, and the current match SHALL get a stronger one. Highlights SHALL NOT change the transcript's layout or text, and SHALL NOT replace the user's text selection.

#### Scenario: Current and other matches
- **GIVEN** two matches are in view and the first is current
- **WHEN** the transcript is shown
- **THEN** both are highlighted, the first more strongly than the second

#### Scenario: Highlight does not move content
- **WHEN** a query that matches visible text is typed
- **THEN** no row of the transcript changes height or position

### Requirement: Find stays live while the session streams
While find is open, text appended or changed in the transcript SHALL update the matches and the count without moving the transcript. The current match SHALL stay the same match while text is only appended.

#### Scenario: Streaming reply adds matches
- **GIVEN** the find bar shows `1 of 2` for `test` and a reply is streaming
- **WHEN** the reply appends a sentence containing `test`
- **THEN** the bar shows `1 of 3` and the current match and scroll position are unchanged

### Requirement: Close find
Escape, while the find bar has focus, and the bar's close button SHALL close the bar, remove every find highlight and return focus to the composer. If the composer is not shown, focus SHALL return to the window. Closing SHALL NOT scroll the transcript. Escape SHALL close the find bar before a second Escape can count toward cancelling a running turn. Opening another session SHALL close the bar.

#### Scenario: Escape closes
- **GIVEN** the find bar is open with highlights in view
- **WHEN** the user presses Escape
- **THEN** the bar closes, no find highlight remains, the composer has focus and the scroll position is unchanged

#### Scenario: Escape during a running turn
- **GIVEN** a turn is running and the find bar has focus
- **WHEN** the user presses Escape twice within 700 ms
- **THEN** the first Escape closes the bar and the turn keeps running

#### Scenario: Switching sessions
- **GIVEN** the find bar is open
- **WHEN** the user opens another session
- **THEN** the find bar is closed

### Requirement: Find covers reasoning and tool content
Find SHALL also search reasoning text, tool-call content shown as Markdown, and tool output shown as code, as each would be displayed when expanded. Notices and compaction summaries SHALL NOT be searched.

#### Scenario: Match in reasoning
- **GIVEN** a reasoning block whose text contains `deadlock`
- **WHEN** the user searches for `deadlock`
- **THEN** the match is counted

#### Scenario: Match in tool output
- **GIVEN** a read tool result whose output contains `fn parse_header`
- **WHEN** the user searches for `parse_header`
- **THEN** the match is counted and highlighted in the code when shown

### Requirement: Moving to a hidden match reveals it
Moving to a match inside a collapsed work group, reasoning block or tool call SHALL expand what hides it so the match is visible. Closing the find bar SHALL leave those sections expanded.

#### Scenario: Match in a collapsed work group
- **GIVEN** a settled work group is collapsed and a tool output inside it contains the next match
- **WHEN** the user presses Enter
- **THEN** the work group and the tool call expand and the match is visible and highlighted as current

#### Scenario: Expansion kept after closing
- **GIVEN** find expanded a reasoning block to show a match
- **WHEN** the user closes the find bar
- **THEN** the reasoning block stays expanded
