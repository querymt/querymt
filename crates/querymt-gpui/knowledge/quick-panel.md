---
id: gpui-quick-panel
title: Quick panel
description: The native-only floating HUD chat - QuickHost state and lifetime, window kinds per platform, global shortcut backends, single instance and --quick, tray, chrome/material per desktop, the edge light and its renderers.
tags: quick panel, hud, floating window, hotkey, global shortcut, tray, single instance, socket, material, blur, glass, edge light, activity, animation, wayland, macos, windows, x11
knowledge_paths: ["crates/querymt-gpui/src/quick/**"]
knowledge_sources: ["crates/querymt-gpui/src/quick/mod.rs", "crates/querymt-gpui/src/quick/panel.rs", "crates/querymt-gpui/src/quick/hotkey.rs", "crates/querymt-gpui/src/quick/instance.rs", "crates/querymt-gpui/src/quick/edge.rs", "crates/querymt-gpui/src/quick/activity.rs"]
---

# Quick panel

A small floating chat opened from any application by a global shortcut (Option+Space on
macOS, Ctrl+Alt+Space elsewhere), off by default (`Prefs.quick.enabled`). Everything in
`src/quick/` is `#[cfg(not(target_family = "wasm"))]`. Design:
`openspec/changes/add-quick-session-panel/design.md` (D7–D18); behaviour: spec `quick-panel`
(still a delta spec in `openspec/changes/add-quick-session-panel/specs/`, not yet in
`openspec/specs/`).

## State and lifetime

- `QuickHost` (`src/quick/mod.rs`, a GPUI global) outlives every window. It owns the
  `Entity<AgentsStore>` (so the store outlives the main window), `QuickState`, the open
  panel if any, the main window handle while known, and the hotkey/tray/socket channel.
- `QuickState { session, draft, picks, hidden_at }`: the strong `Entity<OpenSession>` keeps
  the conversation open while the panel is hidden. `RELEASE_AFTER` (15 min) after a dismiss
  it is released and the next opening starts a new quick chat.
- Every opening builds a **new window and a new `SessionPane`** (Compact density) from
  `QuickState`; closing writes the session and draft back. Text inputs belong to a window,
  so panes are not reused across windows.
- `Picks { model, cwd, reasoning }` are the panel's own: a model picked here never changes
  what the Start page launches with. Quick chats launch locally on the primary agent.
- "Open in main window" (Cmd/Ctrl+Enter) navigates the main pane to the same
  `SessionRef`; the store's hub returns the same `OpenSession`, so nothing reloads.

## Windows per platform

`src/quick/panel.rs` picks the window kind: `PopUp` on macOS and Windows; on Wayland a
`LayerShell` surface where the compositor offers it, else `Normal` (GNOME); `Floating` on X11
(override-redirect `PopUp` windows get no keyboard focus there). Dismissal is on
deactivation, Esc (captured before the composer) or the shortcut. The window grows with
the conversation (bottom edge fixed on macOS via AppKit; top-left fixed on Windows/X11)
and can be dragged; its position is remembered per display in `Prefs.quick.positions`.

## Shortcut, instance, tray

- `src/quick/hotkey.rs`: `Hotkey::{Native, Portal, Unavailable}`. `global-hotkey` on
  macOS, Windows and X11; the GlobalShortcuts portal on Wayland. Presses go through one
  bounded channel to a foreground loop. A new shortcut is registered before the old one is
  dropped, so a taken shortcut leaves the previous one working.
- `src/quick/instance.rs`: one instance per user on a local socket. A later start sends
  `Command::Quick` (`--quick`) or `Command::Show` and exits.
- `src/quick/tray.rs`: Open QueryMT, Quick panel, Quit (ksni on Linux, no GTK). With the
  panel enabled the app uses `QuitMode::Explicit`, so closing the main window does not quit.

## Look and motion

- `src/quick/hud.rs`: a fixed dark HUD palette, whatever the app theme; replaces the
  `Palette` global inside the pane.
- `src/quick/chrome/`: the desktop's own material behind GPUI's view: AppKit
  visual-effect or Liquid Glass on macOS (a sibling view below GPUI's, never a subview),
  Mica/blur on Windows, compositor blur on KDE Wayland, opaque elsewhere. `Chrome`
  describes the installed material for the render code: `glass` (see-through surfaces, the
  pane's main switch), plus `radius`, `tint`, `rimmed`, `native_motion`, `white_through`
  and the root colours from `surfaces`. The window's `Root` must stay transparent or it
  covers the material.
- **The bar of a new quick chat** (D19, `render_empty` in `panel.rs`): a column. The input
  (`quick-composer`, 26 px lines, the kit's `xsmall` size so it adds no padding) has the full
  width, the launch spinner at its trailing end; under it is one row of labels, 12 px text
  set once on the row, whose first label's text starts where the prompt's text does. The
  kit's `Textarea` insets its lines by `Size::XSmall.input_px()` (4 px, `BAR_TEXT_INSET`)
  even without its appearance, so the row is pulled out by the items' 6 px padding less
  those 4 px. The labels are the agent (only when several are configured), `quick-folder`,
  `quick-mode` (a 6 px `mode_tone` dot and the name; the name keeps the selector
  `quick-mode:{label}`; the label is as wide as the widest mode's name, the other names
  laid out at zero height beside it, so cycling the mode moves nothing after it),
  `quick-model` (name and a 10 px chevron),
  `quick-reasoning` (`Brain` and the effort, also at `auto`), and `quick-picker-toggle` at
  the trailing end. Items are 22 px, `p.muted` text and `p.dim` glyphs, no fill at rest,
  `p.chip` and `p.text` on hover, with caps of 90 (agent), 160 (folder) and 220 px (model).
  Each is a button that occludes, so it is not a drag area; the gaps between them are.
  Click on the mode moves to the next one (Shift: the previous), on the reasoning to the
  next effort; the folder and the model open their choosers. `BAR_HEIGHT` is 64: 8 px above
  the input, 26, a 2 px gap, 22, 6 px below (the root's 1 px border counts in the 8 and 6).
  A long prompt wraps at the full width and the bar grows through `bar_h`; nothing stacks
  and the input's height is not measured.
- **A conversation shows what it runs with and has no controls for it** (D20). The header
  (`run_text` in `panel.rs`, data from `SessionPane::run_labels`) has `quick-run-model`, a
  `·` and `quick-run-effort` between the drag area and the context meter: `text_xs` muted
  text, no fill or chevron at rest, the bar's label hover, 60 % and inert while a change is
  pending. Click on the model opens the sheet, on the effort moves to the next one; the
  effort is absent when the agent has no reasoning option. The effort's box is as wide as
  the widest effort name (`RunLabels.efforts`, the others at zero height): the group is
  anchored on its right, so a box as wide as its text moves the model. The compact pill
  has 6 px above and below its one row, so everything in it is on the pill's centre line
  (the main window's composer keeps 4 above, 8 below). The compact composer is plus,
  input, `composer-mode`, send: the mode is its name in a box as wide as the widest mode
  name (`SessionPane::mode_name`, the other names at zero height), keeping the selector
  `composer-mode:{label}`; Click/Shift+click or Tab/Shift+Tab change it. With modes the
  pill has a 1 px outline all round in `mode_tone` at `MODE_OUTLINE_STRENGTH` (`composer-outline`,
  replaces the white top highlight; without modes the highlight stays). There are no model or
  mode chips. The model sheet (`PickerFrame::Sheet`) is the search field and the list only:
  no title, reasoning footer, close button or Done; the reasoning effort changes by
  Cmd/Ctrl+T or the header.
- **Edge light** (D14): `activity.rs` turns what the shown session does into an
  `Activity` whose `phase` is a `Phase` (Running, Waiting, Error, Done, Idle; Done/Error
  are transitions only).
  `edge.rs` turns it into an `EdgeState`; `frame_at` is the definition of the motion and all
  constants live in `MOTION`. Renderers: `chrome/rim.rs` (Core Animation on macOS, driven
  by `edge_plan.rs`, no main-thread work between state changes) or the GPUI-drawn ring as
  the fallback. Renderers implement `EdgeLight`; a failing one is dropped for the ring.
  `activity.rs`, `edge.rs` and `edge_plan.rs` are pure and tested on every platform;
  `chrome/rim.rs` is macOS only.

## Testing

`src/quick/tests.rs` covers summoning, dismissing, quick chats, the session picker and the
hand-off to the main window, using GPUI's test context and clock (`advance_clock`).

## Files

| File | Purpose |
|------|---------|
| `../src/quick/mod.rs` | `QuickHost`, `QuickState`, show/hide/toggle, release timer, enable/disable. |
| `../src/quick/panel.rs` | The panel window: bar, conversation, session/model/folder pickers, sizing. |
| `../src/quick/hotkey.rs` | Global shortcut backends and shortcut recording. |
| `../src/quick/instance.rs` | Single instance socket and `--quick` forwarding. |
| `../src/quick/tray.rs` | Tray icon menu. |
| `../src/quick/hud.rs` | The panel's fixed dark palette. |
| `../src/quick/activity.rs` | Session activity tracker for the edge light. |
| `../src/quick/edge.rs` | Edge light motion model, outline and renderer trait. |
| `../src/quick/edge_plan.rs` | Keyframe plan for renderers that animate by themselves. |
| `../src/quick/chrome/mod.rs` | Platform material install and the `Chrome` flag. |

Related: [ui.md](ui.md) for `SessionPane` and density, [sessions.md](sessions.md) for
session lifetime.
