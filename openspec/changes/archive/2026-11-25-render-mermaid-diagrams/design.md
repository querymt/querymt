# Design

## Context

- **Markdown pipeline:**
  - Transcript Markdown is styled in `md::styled` (`src/ui/md.rs:72`), which every `Kept::markdown` view uses (replies, user messages, reasoning, Markdown tool content) in both the main window and the quick panel.
  - Code fences use the native code block with a header supplied through `code_block_actions`.
  - Tables are already intercepted: `markdown_block_parser` turns `Node::Table` into a custom `MarkdownNode` (`TABLE_BLOCK`), and `markdown_block_renderer` draws a card (`:119-137`). Mermaid follows the same pattern.
- **Row heights:**
  - The transcript is a virtualized list that measures rows when it builds them.
  - `Kept::row_ready` keeps a height provisional only while a Markdown parse is pending. Any element whose size is not known at first layout makes the list jump.
- **Renderer (limu 0.2.1, limu-gpui 0.2.1, both MIT):**
  - `limu::parse(text, &ParseOptions)` gives a `Diagram`. `Diagram::layout(&layout::Options)` gives a `Scene`: the laid-out diagram as data, without colours.
  - Text is measured by a `Measurer`. `limu_gpui::TextMeasurer::new(window, Fonts)` measures with GPUI's text system.
  - `limu_gpui::paint(scene, &Options { look, fonts, .. }, origin, scale, window, cx)` paints in an element's paint phase.
  - Style is a `Look` (a `Theme` plus role overrides, with `theme_only` to ignore author colours). It is applied at paint, so changing the theme does not re-layout.
  - Limits: `DEFAULT_LIMIT` (Mermaid's maxTextSize) and `DEFAULT_MAX_EDGES` (500).
  - limu-gpui depends on `gpui = { package = "gpui-pre", version = "0.3.11" }` and `lyon = "1.0"`. gpui-kit 0.11 pins `gpui-pre =0.3.14` and does not enable its `gpui-fast` feature here (`Cargo.toml:14`), so one GPUI is linked.
  - **Spike result:** limu-gpui 0.2.1 does not compile against gpui-pre 0.3.14, which replaced lyon's `FillOptions`/`StrokeOptions` with its own types. The D6 fallback was taken: only `limu` is a dependency and the painter is ported into `src/ui/mermaid/`.
  - Rotated text and halos are painted with `window.paint_svg`, shaped by usvg with system fonts and bundled font assets.

## Goals / Non-Goals

**Goals:**
- A diagram's size is final at first layout, so the transcript never jumps.
- Layout runs at most once per (source, fonts). Theme changes, resizes, zoom and pan are paint-only.
- A large diagram can be read in place without the wheel ever getting stuck on it: the transcript keeps plain scrolling.
- Rendering degrades to today's code block on any failure.

**Non-Goals:**
- Making text inside diagrams selectable. Find highlights inside diagrams are a non-goal in `add-session-find`.
- Running layout off the main thread. limu's layouts of typical diagrams take milliseconds. Revisit if the timing test in tasks shows otherwise.

## Decisions

### D1. Intercept only closed `mermaid` fences, in the block parser

- **Interception:**
  - In `markdown_block_parser`, a `Node::Code` whose `lang` trimmed and lowercased is `mermaid`, and whose node source ends with a closing fence line (the same fence character, repeated at least as often as the opener), becomes `MarkdownNode::new(MERMAID_BLOCK, body)`.
  - The node is built with `.text(body)`, so plain copy and `rendered_text()` include the source (as tables do).
- **Open fences:** an unclosed fence stays a native code block. This is what makes streaming safe. mdast reports an unclosed fence as a code node that runs to the end of the document, so the closing-fence check is what tells the two apart.
- **Alternative considered:** rendering inside `code_block_actions`. That slot is a header, not a replacement for the body, and nesting would put a diagram inside a code frame with the code still laid out.

### D2. A diagram element whose size comes from a cached layout

- **Element:** `mermaid::DiagramElement` is a custom `Element` with an id derived from the view id and the block's source start, as `table_card` ids are. It is also handed the block's viewport store (D5, D7).
  - `request_layout` uses `window.request_measured_layout` with the available width:
    - It fetches or computes the cached `Scene` for the key (D3).
    - `fit = min(1, available_w / scene_w, MAX_H / scene_h)` with `MAX_H = 480px`, and `size = (scene_w * fit, scene_h * fit)`. This is the diagram area; zoom (D7) never changes it.
  - **Flex minimum width:** the card body is a flex row, and a flex item with `min-width: auto` is never narrower than its min-content width. The element therefore sets `min_size.width = 0` and answers a min-content measure with 0 (a diagram can always scale down). Without this, the min-content measure returned the natural width, the diagram never shrank, and the card's `overflow_hidden` clipped it on both sides (it is centred). A test in a plain block container does not catch this; the regression test lays the diagram out in the real card.
    - Layout happens when the card renders (`mermaid::scene(source, window, cx)`, a cache miss only the first time), because `TextMeasurer` needs the `Window` and the card must know before building its body whether the diagram can be drawn. The element is then handed the cached `Rc<Scene>`, so `request_layout` only measures it.
  - `paint` calls `limu_gpui::paint` with the scale, the bounds' origin and the current `Look` (D4).
- **Why:** the size is known at first layout, so `Kept::row_ready` needs no new state. A resize only changes `scale`.
- **Alternative considered:** rendering limu's SVG through GPUI's `svg()` or `img()`. The SVG would be measured with Arial metrics rather than the app's fonts (labels clip or overflow), it would rasterize per size, and it would add a render round trip.

### D3. Layout cache

- **Contents:** a process-wide LRU of 128 entries, `HashMap<u64, Rc<Result<Scene, DiagramError>>>`. It is shaped like `code.rs`'s highlight cache (`CACHE_LIMIT`).
- **Key:** the hash of (source, sans family, mono family, base font size).
- **Failures:** they are cached too, so a broken diagram is not re-parsed every frame.
- **Lifetime:** the cache is not cleared on session switch. The same diagram reappearing (forks, reloads) is common.

### D4. Look from the palette

- **Mapping:** `mermaid::look(p: &Palette) -> Look` sets `theme_only = true` and maps the palette onto limu's `Theme`:
  - background token `p.card`, opaque and equal to the card's fill, because limu also uses it to back edge labels (a transparent value would let lines run through labels)
  - text and foreground `p.text`
  - lines `p.muted`
  - node fill `p.surface`
  - node border `p.border`
  - surface-alt `p.card_hover`, accent `p.accent`, critical `p.danger`, and the eight series colours from the palette's accent, status and muted colours.
- **When it is built:** at paint from `crate::theme::palette(cx)`, so the dark/light switch only recolours.
- **Fonts:** `Fonts { sans: "IBM Plex Sans", mono: <the theme's mono family> }`.
- **Why `theme_only`:** authors' colours are tuned for Mermaid's default white page and are often unreadable in dark mode. The spec requires ignoring them.

### D5. Card, toggle and fallback

- **Card:** `mermaid_card` reuses the code-block frame and header styling from `md::styled`, factored out so both share it:
  - the label `mermaid`
  - a two-state segmented toggle (Diagram / Source)
  - `CopyButton` with the body.
- **Source mode:** shows the body through `code::HighlightedLines` with no language. That matches the plain code look without nesting a `TextView`.
- **Zoom controls:** while the diagram is shown, the header also has `−`, the zoom percentage, `+` and `Fit` (D7), before the toggle. They are hidden in Source mode and in the fallback.
- **Card state:** kept in a `Kept`-owned `Arc<Mutex<CardModes>>` (the block renderer closure must be `Send + Sync`, so an `Rc` cannot be captured), so it survives rows being rebuilt and lasts while the session is open. `CardModes` holds the `source` set of block ids shown as source and `views: HashMap<String, Viewport>` (D7). `Kept::clear` resets both on session switch.
- **Fallback:**
  - `parse` or `layout` errors, `Diagram::diagram_type` not supported (`limu::supports`) and limit errors all render the Source mode, plus a single muted line `Diagram not drawn: <reason>`.
  - The reason comes from limu's `ErrorCode`, mapped to short text: syntax error, unsupported diagram type, too large.
  - The toggle is hidden in this state.

### D6. Spike first: dependency and target check (fallback taken, see the spike result under Context)

Before building anything, add the dependencies and prove:
1. `cargo tree -d` shows a single `gpui-pre`.
2. Native and `wasm32-unknown-unknown` builds pass.
3. limu-gpui paints a sample in a GPUI test window.

The spike also confirms the exact `Theme` fields and `Scene` size accessors that D2 and D4 rely on.

If the spike fails on the GPUI version or the wasm target, the fallback is:
- keep `limu` for parse and layout, measured with a `TextMeasurer` of our own (the trait is tiny);
- port the painter (about 800 lines, MIT) into `src/ui/mermaid.rs` against gpui-kit's re-exports.

This keeps the same design without the crate.

### D7. Zoom and pan inside the card

- **State:** `Viewport { zoom, offset }` per block, in `CardModes.views` (D5). `zoom` is relative to the fitted scale, so the paint scale is `fit * zoom` and a resize keeps the zoom. A block without an entry is at Fit (`zoom = 1`, centred).
- **Paint only:** zoom and pan change the paint origin (`bounds.origin + offset`) and scale inside the existing content mask. They never re-run layout or change the element's size, so the row height is stable (spec: "Zoom and pan leave the transcript alone").
- **Zoom math:** wheel and pinch zoom keep the point under the pointer fixed: `offset' = p - (p - offset) * (new / old)`. The range is `1 ..= max(4, 2 / fit)` in `zoom` units, that is, from Fit up to the greater of four times Fit and twice the natural size. A pixel wheel delta zooms by `exp(dy * 0.0025)` (GPUI's `dy` is positive for scrolling up, which zooms in), a line-mode notch by about 10% per line, and a pinch by `1 + delta`. The `−` and `+` buttons step by 25% around the area's centre.
- **Pan:** a left-button drag over the diagram pans it while `zoom > 1`. The offset is clamped so the diagram's edges cannot pass the area's edges, and an axis on which the diagram is smaller than the area stays centred.
- **Who owns the wheel:** only Cmd+wheel (Ctrl+wheel off macOS). The element inserts a hitbox in `prepaint`, and in `paint` registers a bubble-phase `ScrollEvent` handler that, when its hitbox handles the scroll and the modifier is held, zooms and calls `stop_propagation`. The transcript `list` registers its scroll handler before painting its children, so the child runs first and the list never sees the event. Without the modifier the handler does nothing, and the event reaches the transcript.
- **Smooth scroll:** `SmoothScroll` swallows wheel notches in the capture phase and replays them over later frames with the modifiers held at that moment, so releasing Cmd mid-notch would turn the rest of a zoom into transcript scrolling. It now lets events with the platform or control modifier through untouched.
- **Pinch:** `PinchEvent` over the hitbox zooms around its position. Only the macOS platform emits it; other platforms use the modifier wheel and the buttons.
- **Drag and selection:** a left mouse-down over the hitbox calls `GlobalState::suppress_text_selection` (as `inline_object` does), so the window-level text selection does not start, then tracks the drag with mouse-move and mouse-up handlers. Like any press, it clears an existing selection. A double-click returns to Fit. The cursor is Grab over a pannable diagram and Grabbing during a drag.
- **Refresh:** a change updates the store and calls `window.refresh()`.
- **Measures for the header:** the zoom controls are built before the element is laid out, so the element writes its `Metrics` (fit and area size) to the store in `prepaint` and refreshes when they change. The percentage (`fit * zoom`), the limits and the buttons' centre come from them.
- **Alternatives considered:**
  - *A Fit / 100% toggle with the diagram in a GPUI scroll div.* Implemented first and rejected: a GPUI scroll div never stops propagation of the wheel event, so the transcript list scrolled along with the diagram.
  - *A plain scroll pans a zoomed diagram and passes to the transcript at the edges.* Rejected: scrolling gets stuck while the pointer is over a zoomed diagram.
  - *An expanded viewer overlay.* Deferred to a follow-up; the inline controls cover reading a large diagram.

## Risks / Trade-offs

- [limu is young and its API may change in any 0.x release] → Pin `~0.2`, keep all limu calls inside `src/ui/mermaid.rs`, and cover them with tests that fail on API drift.
- [Rotated labels and halos use usvg with system fonts, which are absent on the wasm build] → Rotated labels may be missing on web. They are rare (axis titles, swimlane names). Note this in the knowledge docs, and if it is unacceptable, ship with the IBM Plex font bytes registered with usvg through limu-gpui's asset path.
- [gpui-kit later enables `gpui-fast`, making limu-gpui link a different GPUI] → The spike's `cargo tree -d` check becomes a CI-visible failure (two GPUIs cause type errors). The fallback in D6 applies.
- [A very wide or tall diagram scaled to fit becomes too small to read] → Zoom and pan (D7). The Source toggle stays available, and a separate viewer is a possible follow-up change.
- [The modifier-wheel and pinch gestures are not visible] → The zoom buttons work without them, and their tooltips name the gestures.
- [Pinch is only reported on macOS] → Other platforms zoom with Ctrl+wheel and the buttons.
- [Dragging a very large diagram repaints the whole scene every frame] → Layout is not re-run; check the 400-edge flowchart by hand, and cull items outside the area if dragging stutters.
- [Synchronous layout of a huge diagram stalls a frame] → limu's limits bound it, and a timing test with a 400-edge flowchart must stay under 16 ms in release. Otherwise move layout to `cx.background_spawn` with a provisional height, accepting one re-measure.
- [`add-session-find` reconstructs rendered text from the Markdown source] → A diagram's text is its body (D1, `.text(body)`), as for a native code block without its fence. Its corpus test gains a mermaid fixture once both changes land.
