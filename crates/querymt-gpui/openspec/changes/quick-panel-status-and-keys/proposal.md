# Proposal

## Why

The quick panel hides state the main window shows, and it is less keyboard-driven than the rest of the app.

- **Agent mode:** compact density drops the Options control, so the panel never shows the agent mode. Tab still cycles it, which changes the mode with nothing on screen.
- **Context usage:** the main window's context ring is gone, so a long quick conversation runs into its limit without warning.
- **Session list:** every other control in the panel has a shortcut; the session list does not.
- **New chat:** Cmd/Ctrl+N does nothing in the panel.
- **Tooltips:** they print the literal "Cmd/Ctrl+M" instead of the platform's notation.

## What Changes

- **Session list shortcut:** Cmd+K on macOS, Ctrl+K elsewhere, toggles the session list in the panel. Its triggers (the header title and the bar's button) show the shortcut in their tooltips.
- **New quick chat shortcut:** Cmd/Ctrl+N starts a new quick chat, as the header's button does.
- **Platform-correct shortcut hints:** the panel's tooltips render shortcuts from the actual key bindings in the platform's notation (⌘K on macOS, Ctrl+K elsewhere).
- **Agent mode chip** (revised by `add-quick-session-panel` D20: in a conversation, the mode's name in the composer):
  - In a conversation, a chip next to the model chip shows the session's mode. Clicking it or pressing Tab cycles the mode, and Shift+Tab goes back.
  - In a new quick chat's bar, the same chip shows the mode the quick chat will start in, and it can be changed for that chat only, without changing the start page's launch mode.
- **Context usage indicator:** in a conversation, the header shows the main window's context ring (same thresholds and tooltip). From 70% it also shows the percentage as text.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `quick-panel`: keyboard shortcuts for the session list and a new quick chat; the agent mode shown and changeable; context usage shown. The capability is introduced by the in-flight `add-quick-session-panel` change, so this change adds requirements, as `quick-picker-progress` does.

## Impact

- **Code:**
  - `src/ui/keys.rs`: new `ToggleSessionList` action bound to `cmd-k` / `ctrl-k`, and a shortcut-label helper.
  - `src/quick/panel.rs`: action handlers for `ToggleSessionList` and `NewSession`, the header context ring, the bar's mode chip, and tooltips through the helper.
  - `src/quick/mod.rs`: `Picks.mode`.
  - `src/store.rs`: `LaunchRequest.mode`.
  - `src/ui/session_view.rs`: the compact composer's mode chip, and the context ring extracted for reuse.
  - Tests in `src/quick/tests.rs` and `src/ui/session_view.rs`.
- **Keymap:** `cmd-k` / `ctrl-k` is unused in the app and in the kit's input bindings. It is bound globally but handled only by the panel; the main window ignores it.
- **Depends on** `add-quick-session-panel`; archive after it.
