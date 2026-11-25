---
id: gpui-ui
title: UI shell, pages and the session pane
description: How the main window is put together (QmtApp, pages, sidebar, modals), how one SessionPane renders a session in both the main window and the quick panel (density, RenderCtx, row cache), and the shared pieces (model picker, keys, palette, fuzzy search).
tags: ui, gpui, shell, pages, sidebar, session pane, transcript, rows, density, compact, render, model picker, keybindings, actions, theme, palette, search
knowledge_paths: ["src/ui/**", "src/theme.rs", "src/search.rs"]
knowledge_sources: ["src/ui/mod.rs", "src/ui/session_pane.rs", "src/ui/model_picker.rs", "src/ui/keys.rs#bind_keys"]
---

# UI shell, pages and the session pane

GPUI via `gpui-kit` (which re-exports gpui, gpui-base, gpui-component and Lucide icons at
matching versions). The design and coding rules for GPUI Kit apps are in the
`gpui-kit` and `gpui-kit-design-guides` skills under `.agents/skills/`.

## The shell

- `QmtApp` (`src/ui/mod.rs`) is the main window's root view. It holds
  `Entity<AgentsStore>`, the current `Page` (`Start`, `Inbox`, `Sessions`, `Automations`,
  `Agents`, `Mesh`, `Settings`, `Session`, `Setup`, `Workspaces`), page state, and the main
  `SessionPane`. It observes the store and re-renders on list-level changes.
- `ui::open_main_window(store, cx)` builds the main window; it is used at start and by the
  quick panel's "open in main window" when the main window is closed.
- The window frame is GPUI Kit's; the app owns only its header (`src/ui/chrome.rs`).
- Pages are mostly one file each (`start.rs`, `sessions.rs`, `agents.rs`, `settings.rs`,
  `automations.rs`, `mesh.rs`, `workspaces.rs`, `setup.rs`, `inbox.rs`). Dialogs are the
  app's own centred modals. `src/ui/kit.rs` holds small shared presentational helpers.

## SessionPane: one session surface, two densities

`SessionPane` (`src/ui/session_pane.rs`) shows one session and owns everything a surface
remembers about it: the strong `Entity<OpenSession>` handle that keeps it open, the back
stack of delegated sessions (`Vec<SessionRef>`), the transcript list, expanded rows, the
composer, attachments and the request-card drafts. Two panes showing the same session share
the `OpenSession` and nothing else.

- `Density::Comfortable` is the main window; `Density::Compact` is the quick panel
  (collapsed tool/reasoning rows, smaller output caps, hover actions, a one-line composer,
  no usage ring, details or options panel). Density changes defaults and limits; there is
  no separate row code and no trait over the host.
- What belongs to the app leaves as a `SessionPaneEvent` (`Leave`, `Fork`, `OpenImage`,
  `ConfirmUndo`, `Worktree`, `DebugEvents`, `Delegates`, `ModelPicker`, `NewSessionIn`,
  `ConnectorSettings`, `OpenInMain`), handled by `QmtApp` or `QuickPanel`. Opening a
  delegated child session is the pane's own (its back stack), not an event.

## Transcript rendering

`src/ui/session_view.rs` holds the row renderers as free functions over a `RenderCtx`
(palette, syntax theme, `WeakEntity<SessionPane>`, density, expanded set, turn info). The
list is virtualized; row signatures are built from `Item.key`, `Item.rev` and the
`Arc<ToolCall>` pointer, so a row re-measures only when its item changed. Keep that
invariant when touching the projection (see [sessions.md](sessions.md)). Markdown is
`src/ui/md.rs`, highlighted code `src/ui/code.rs` (cached by language, text and theme),
turn diffs `src/ui/turn_diff.rs`, slash commands `src/ui/slash.rs`.

## Shared pieces

- **Model picker** (`src/ui/model_picker.rs`): one `ModelPicker` entity for the main
  window and the quick panel. Every pick emits `ModelPickerEvent::Picked { key }`. For
  `Target::Session` the picker has already set the session's model; for
  `Target::Launch { agent }` the event is all that happens and the caller decides (Start
  writes the launch preference, the quick panel keeps it for its next chat).
- **Keys** (`src/ui/keys.rs`): actions (`NewSession`, `ToggleCommandPalette`,
  `UndoWorkspace`, `OpenModelPicker`, `OpenSidebarSession { index }`, …) bound with no key
  context so they reach `QmtApp`'s root even with nothing focused; Cmd on macOS, Ctrl
  elsewhere. Text inputs keep their own deeper bindings.
- **Theme** (`src/theme.rs`): QueryMT design tokens applied to the gpui-component theme,
  plus a `Palette` global for app colours the component theme has no slot for. The
  transcript reads the palette once per frame and passes it down, so the quick panel can
  substitute its HUD palette.
- **Search** (`src/search.rs`): one frizbee-based fuzzy matcher for every search box. Every
  query word must match within one field (its characters in order, gaps allowed);
  different words may match different fields, and a match in a secondary field
  (provider, path, description) counts half (`rank_tiered`).
- **Smooth scroll** (`src/ui/smooth_scroll.rs`): window-wide easing of mouse-wheel notches.

## Files

| File | Purpose |
|------|---------|
| `../src/ui/mod.rs` | `QmtApp` root view, `Page`, modals, `open_main_window`. |
| `../src/ui/session_pane.rs` | `SessionPane`: one session in a surface, at a density. |
| `../src/ui/session_view.rs` | Session page, transcript row renderers, composer, `Density`. |
| `../src/ui/sidebar.rs` | Session tabs in the sidebar: order, drag, context menu, attention dots. |
| `../src/ui/start.rs` | Start page composer and launch. |
| `../src/ui/inbox.rs` | Permission and elicitation cards (Inbox page and inline). |
| `../src/ui/model_picker.rs` | Shared model picker with reasoning effort. |
| `../src/ui/keys.rs` | Key bindings and the command palette. |
| `../src/ui/kit.rs` | Small shared presentational helpers. |

Related: [quick-panel.md](quick-panel.md) for the compact surface.
