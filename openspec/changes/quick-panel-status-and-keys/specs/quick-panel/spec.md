# Spec Delta

## ADDED Requirements

### Requirement: Session list by keyboard
Cmd+K on macOS and Ctrl+K elsewhere SHALL open the panel's session list with its search focused, and SHALL close it again, returning the focus to the composer. It SHALL work both in a new quick chat's bar and in a conversation.

#### Scenario: Open the session list
- **GIVEN** the panel shows a conversation and the composer has focus
- **WHEN** the user presses Cmd/Ctrl+K
- **THEN** the session list opens with its search focused

#### Scenario: Close it again
- **GIVEN** the session list is open
- **WHEN** the user presses Cmd/Ctrl+K
- **THEN** the list closes and the keyboard is on the composer when it is shown, and on the panel otherwise (a read-only conversation), so Esc and the shortcuts keep working

### Requirement: Model picker by keyboard
Cmd+M on macOS and Ctrl+M elsewhere SHALL open the model picker, and SHALL close it again when it is open, as Cmd/Ctrl+K does for the session list.

#### Scenario: Cmd+M twice
- **GIVEN** the model picker is open
- **WHEN** the user presses Cmd/Ctrl+M
- **THEN** the picker closes and the keyboard is on the composer when it is shown, and on the panel otherwise (a read-only conversation), so Esc and the shortcuts keep working

### Requirement: One sheet at a time, and the keyboard never lost
The session list, the model picker and the folder chooser SHALL take the conversation's place one at a time: opening one SHALL close any other and move the keyboard to the one opened. In any order of these keys, Esc SHALL close the open sheet and then the panel, and the panel's shortcuts SHALL keep working. The keyboard SHALL NOT be left on an element that is not shown.

#### Scenario: Model picker, then session list
- **GIVEN** the model picker is open
- **WHEN** the user presses Cmd/Ctrl+K
- **THEN** the model picker closes, the session list is open with its search focused, and Esc closes the list

#### Scenario: Session list, then model picker
- **GIVEN** the session list is open
- **WHEN** the user presses Cmd/Ctrl+M
- **THEN** the session list closes and the model picker is open

#### Scenario: Alternating keys
- **WHEN** the user presses Cmd/Ctrl+M and Cmd/Ctrl+K alternately, in a conversation or in a new quick chat
- **THEN** at most one sheet is open after each key, the keyboard is inside the panel, and Esc, Esc closes the sheet and then the panel

### Requirement: New quick chat by keyboard
Cmd+N on macOS and Ctrl+N elsewhere SHALL start a new quick chat in the panel, exactly as the header's new-chat button does, and SHALL NOT open the main window.

#### Scenario: New quick chat from a conversation
- **GIVEN** the panel shows a conversation
- **WHEN** the user presses Cmd/Ctrl+N
- **THEN** the panel shows an empty quick chat bar with the composer focused, and the main window is unchanged

### Requirement: Shortcut hints in the platform's notation
Every tooltip in the panel that names a shortcut SHALL show the key actually bound to that action, in the platform's notation (for example ⌘K on macOS and Ctrl+K on Windows and Linux).

#### Scenario: Tooltip on macOS
- **WHEN** the user hovers the model label in the bar on macOS
- **THEN** its tooltip shows ⌘M, not "Cmd/Ctrl+M"

### Requirement: Agent mode shown in the panel
The panel SHALL show the agent mode as its name: in a conversation the session's mode, in the composer; in a new quick chat the mode the chat will start in, in the bar. Clicking it, or Tab and Shift+Tab in the composer, SHALL move to the next or previous mode. A mode chosen before the first prompt SHALL apply to that quick chat only, without changing the start page's launch mode. When the agent offers no modes, nothing SHALL be shown.

#### Scenario: Mode in a conversation
- **GIVEN** the panel shows a session in the "Plan" mode
- **THEN** the composer shows "Plan"

#### Scenario: Tab changes the mode visibly
- **GIVEN** the panel shows a session in the "Build" mode and the composer has focus
- **WHEN** the user presses Tab
- **THEN** the session moves to the next mode and the composer shows its name

#### Scenario: Mode for the next quick chat
- **GIVEN** the panel is on an empty quick chat and the start page's launch mode is "Build"
- **WHEN** the user changes the chip to "Plan" and sends a prompt
- **THEN** the new session starts in "Plan", and the start page's launch mode is still "Build"

### Requirement: Context usage shown in the panel
While a conversation is shown and its context use and size are known, the panel's header SHALL show how full the context is as a ring, coloured as in the main window (normal, warning above 70%, danger above 90%), with a tooltip giving the tokens used, the size and the percentage. From 70% the percentage SHALL also be shown as text next to the ring. When the size is unknown, nothing SHALL be shown.

#### Scenario: Context half full
- **GIVEN** the session shown has used 50% of its context
- **THEN** the header shows the ring at half, in the normal colour, without a percentage label, and its tooltip gives the token counts and "50%"

#### Scenario: Context nearly full
- **GIVEN** the session shown has used 92% of its context
- **THEN** the ring is in the danger colour and "92%" is shown next to it

#### Scenario: Unknown context size
- **GIVEN** neither the agent nor the model list gives the session's context size
- **THEN** no ring is shown
