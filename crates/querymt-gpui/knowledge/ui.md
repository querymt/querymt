---
id: gpui-ui
title: UI shell, pages and the session pane
description: How the main window is put together (QmtApp, pages, sidebar, modals), how one SessionPane renders a session in both the main window and the quick panel (density, RenderCtx, row cache), and the shared pieces (model picker, keys, palette, fuzzy search).
tags: ui, gpui, find, search in session, cmd-f, mermaid, diagrams, markdown, shell, pages, sidebar, session pane, transcript, rows, density, compact, render, model picker, keybindings, actions, theme, palette, search
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

## Mermaid diagrams

Closed `mermaid` fences in transcript Markdown are drawn as diagrams (spec:
`openspec/specs/mermaid-diagrams`; history in `openspec/changes/archive/2026-11-25-render-mermaid-diagrams`).

- **Interception** (`src/ui/md.rs`, `styled`): the block parser turns a `Node::Code` whose
  language is `mermaid` (case-insensitive) into the custom `MERMAID_BLOCK` with
  `.text(body)`, but only when `fence_closed` finds the closing fence (same fence character,
  at least as long as the opener). An open fence is an unclosed code node running to the end
  of the document, so it stays a native code block while streaming and becomes a diagram when
  the closing fence arrives. Other languages are untouched.
- **Card** (`MermaidCard` in `md.rs`): the code block's frame and `header_row` (shared with the
  code-block header), with `mermaid`, the zoom controls (`−`, the percentage of natural size,
  `+`, `Fit`; diagram view only, tooltips name the gestures), a Diagram / Source toggle and
  `CopyButton`, which always copies the body. Card state is a `CardModes` owned by `Kept`
  (`SourceShown`, an `Arc<Mutex<_>>` because the block renderer must be `Send + Sync`): the
  `source` id set and `views`, the `mermaid::Views` store of zoom/pan and measures. It survives
  rows being rebuilt and is cleared by `Kept::clear`. The old Fit / 100% toggle, its scroller,
  `diagram_actual`, `natural_size` and `ACTUAL_MAX_HEIGHT` are gone.
- **Fit and height cap**: a diagram starts at `mermaid::fit_scale`, the largest scale no greater
  than 1 that fits the card's width and `MAX_H` = 480px, never scaled up. The diagram area is
  that size, so the row height is final at first layout (a 1200px diagram is a 480px area at 40%).
- **Zoom and pan** (`Viewport { zoom, x, y }` per block, design D7): `zoom` is relative to Fit
  (so a resize keeps it), `(x, y)` is the paint offset inside the area. Range `1 ..= max(4,
  2 / fit)`. Pure maths (`zoom_about` keeps the pointer's point fixed, `clamp` limits the offset
  to the diagram edges and centres an axis it does not fill) is unit tested. Zoom and pan change
  only the paint origin and scale inside the content mask: never layout, never the element's
  size, so the card and the rows below do not move. `ViewHandle` is one block's entry in the
  shared store; the element writes its `Metrics` (fit, area) in `prepaint` and refreshes when
  they change, which is how the header buttons know the centre, limits and percentage.
- **Wheel ownership**: only Cmd+wheel (Ctrl off macOS, `Modifiers::secondary`) and trackpad
  pinch belong to the diagram. `DiagramElement::register_input` adds bubble-phase handlers over a
  hitbox; the transcript `list` registers its scroll handler before painting its children, so the
  child runs first and `stop_propagation` keeps the list from scrolling. A plain wheel or
  scroll does nothing there and reaches the list, so scrolling never sticks on a diagram. Wheel up
  zooms in (`exp(dy * 0.0025)` per pixel, 1.1 per line notch; the design's sign was DOM
  convention), a pinch by `1 + delta`, the buttons by 1.25 about the area's centre.
- **Smooth-scroll bypass** (`SmoothScroll::install`, `smooth_scroll.rs`): the capture handler
  swallows wheel notches and replays them over later frames with whatever modifiers are held then,
  so releasing Cmd mid-notch would turn a zoom into transcript scrolling. Events with the
  platform or control modifier are now left alone (and drop any running scroll tail).
- **Drag and selection**: a left press over the hitbox calls
  `GlobalState::suppress_text_selection` (as buttons do) so the window-level selection does not
  start; a drag pans while `zoom > 1` (cursor Grab / Grabbing, drag state in `Views`); a
  double-click returns to Fit.
- **Fallback**: a parse error, unsupported type or limit error (`mermaid::Unavailable`) shows
  the source as plain lines plus `Diagram not drawn: <reason>`, with the toggle hidden. Never
  an empty card.
- **Layout and cache** (`src/ui/mermaid.rs`): `mermaid::scene` parses and lays out with `limu`,
  measured by GPUI's text system (`paint::TextMeasurer`) in the app fonts, and caches the
  result (a 128 entry LRU keyed by source and fonts; failures are cached too). The card calls
  it while rendering, so a diagram's size is known when its row is first laid out and the
  transcript does not jump.
- **Element**: `DiagramElement` reports the scene size scaled down to the available width and
  the height cap, never up; a resize only changes the fit. It sits in a flex row, where an item is
  no narrower than its min-content width, so it has `min_size.width = 0` and answers a
  min-content measure with 0 (otherwise it kept its natural width and the card cut it off on
  both sides).
- **Theme**: `mermaid::look(&Palette)` maps the palette onto limu's `Theme` with
  `theme_only`, so author colours are ignored. It is applied at paint, so a theme switch
  recolours without re-laying out.
- **Painter**: `src/ui/mermaid/paint.rs` and `svg.rs` are a port of `limu-gpui` 0.2.1 (MIT),
  which does not build against the `gpui-pre` 0.3.14 that gpui-kit pins (no lyon path
  options). Only `limu` is a dependency; all limu calls stay inside `src/ui/mermaid`. Revisit
  when a limu-gpui release supports the pinned GPUI.
- **Known limitation (wasm)**: rotated labels and halos are painted as SVG text shaped by
  usvg, which uses system fonts that the web build lacks, so they may be missing there
  (axis titles, swimlane names).
- **Find**: a diagram's text is its body (`.text(body)`), so `add-session-find` counts matches
  in it but cannot paint highlights inside the card.

## Find in session

Cmd+F (Ctrl+F elsewhere) opens a find bar over the main window's transcript (OpenSpec change
`add-session-find`; "design Dn" below is that change's `design.md`). Tier 1 searches user messages and agent replies. Tier 2 (reasoning, tool output,
auto-expansion) is not built yet.

Two layers, because the transcript is virtualized and rows outside the viewport have no
elements (D1):

- **Data** (`src/ui/find.rs`): `FindIndex` counts and orders matches from the session's
  `Item`s without building any element. `views_of(item)` lists the Markdown views an item
  shows (`umd-{key}` for a user message with front matter split off and trimmed, `md-{key}`
  for a reply) and the row renderers take their ids and text from the same helpers
  (`user_view_id`, `reply_view_id`, `user_body`), so search cannot drift from rendering (D2).
  `searchable_text` rebuilds `TextViewState::rendered_text()` from the Markdown tree (soft
  breaks become spaces, inline tags vanish, tables and Mermaid keep their source because they
  are custom blocks); a corpus test in `md.rs` renders fixtures and requires equal match
  counts. `FindIndex` caches each item's `(rev, counts)`, so a streaming notify re-reads only
  the item whose `rev` changed (D3). The current match is a `FindMatch { item_key, view_id,
  occurrence }`, found again after every recompute; if it is gone the next match in order
  takes over.
- **Paint** (`src/ui/md.rs`): `Kept::set_find` holds a `FindQuery` (query, current match,
  two accent alphas). `Kept::state` runs the same `Matcher` over a built view's
  `rendered_text()` and calls `set_range_highlights` and `reveal_range`. A per-view stamp
  (find generation, `RenderedText`) makes this idempotent, so render never notifies in a
  loop. A missing occurrence paints without a current match and reveals the first one (D2).
  Tables and Mermaid blocks are counted but not painted (gpui-base leaves custom blocks
  unpainted); the reveal still scrolls to them.

The matcher (`Matcher`, `match_ranges`) is a case-insensitive literal match. A whitespace run
in the query matches spaces and tabs, never a newline, so a match stays in one block. Case
folding is per char with a byte map back into the text, so offsets stay on char boundaries.

The bar and its state live on `SessionPane` (`find: Option<FindBar>`, in `find.rs`):

- **Keys** (D5): `FindInSession` in `keys.rs` is bound to `cmd-f` / `ctrl-f` with no context.
  `QmtApp` handles it only for `Page::Session` with the palette closed. The composer is not
  searchable, so gpui-base's `Input` binding for the same key propagates to it. The quick
  panel's root does not handle the action, so the shortcut does nothing there.
- **Bar**: a popover-class surface (`popover` fill, hairline, overlay shadow) at the
  transcript's top right, with the query `Input`, `n of m` / `No results`, previous, next and
  close buttons (ids `find-bar`, `find-label`, `find-prev`, `find-next`, `find-close`). The
  arrows are disabled with no matches. Enter and Shift+Enter step (`InputEvent::PressEnter {
  shift }`). Enter also reports a `Change` with the text unchanged, so `find_query_changed`
  ignores a change that does not alter the query.
- **Navigation** (D4): `scroll_to_reveal_item` for the current match's row, then
  `pause_following_tail`; the highlight pass reveals the line inside a tall reply. On a query
  change the current match is the first one at or below the item at the viewport's top (else
  the first overall). Typing therefore picks a match near the reading position, and a
  transcript that is following its tail starts from the first match overall.
- **Closing**: the bar's `capture_action(Escape)` closes it and stops the key, so the app's
  double-Escape tracker in `mod.rs` never counts that press. Closing clears every highlight
  (`Kept::set_find(None)`), keeps the scroll position and returns focus to the composer, or
  blurs the window when the session takes no input. `SessionPane::show` and `clear` close it.
- **Streaming**: the pane's session observer calls `sync_find`, which only runs while the bar
  is open and never scrolls.

## Shared pieces

- **Model picker** (`src/ui/model_picker.rs`): one `ModelPicker` entity for the main
  window and the quick panel. Every pick emits `ModelPickerEvent::Picked { key }`. For
  `Target::Session` the picker has already set the session's model; for
  `Target::Launch { agent }` the event is all that happens and the caller decides (Start
  writes the launch preference, the quick panel keeps it for its next chat).
- **Badges** (`src/ui/kit.rs`): `badged` puts an optional left and right tag
  (`badge_tag`) on a control's top edge, at a `BadgeSize`. The main window's model pills
  use `badged_tone` (one tag at the right, `BadgeSize::TOOLBAR`, cut out with `surface`);
  the quick panel's bar passes both tags as buttons (`BadgeSize::HUD`).
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
| `../src/ui/md.rs` | Transcript Markdown: styling, code and table cards, the Mermaid card, `Kept` (and its find highlights). |
| `../src/ui/find.rs` | Find in session: matcher, searchable text, `FindIndex`, and the find bar on `SessionPane`. |
| `../src/ui/mermaid.rs` | Mermaid layout cache, `DiagramElement` (fit, zoom, pan), viewport maths, palette look; `mermaid/paint.rs` and `svg.rs` paint limu scenes with GPUI. |

Related: [quick-panel.md](quick-panel.md) for the compact surface.
