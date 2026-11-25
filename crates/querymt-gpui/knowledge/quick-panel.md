---
id: gpui-quick-panel
title: Quick panel
description: The native-only floating HUD chat - QuickHost state and lifetime, window kinds per platform, global shortcut backends, single instance and --quick, tray, chrome/material per desktop, the edge light and its renderers.
tags: quick panel, hud, floating window, hotkey, global shortcut, tray, single instance, socket, material, blur, glass, edge light, activity, animation, wayland, macos, windows, x11
knowledge_paths: ["src/quick/**"]
knowledge_sources: ["src/quick/mod.rs", "src/quick/panel.rs", "src/quick/hotkey.rs", "src/quick/instance.rs", "src/quick/edge.rs", "src/quick/activity.rs"]
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
