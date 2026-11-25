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
  - Rotated text and halos are painted with `window.paint_svg`, shaped by usvg with system fonts and bundled font assets.

## Goals / Non-Goals

**Goals:**
- A diagram's size is final at first layout, so the transcript never jumps.
- Layout runs at most once per (source, fonts). Theme changes and resizes are paint-only.
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

- **Element:** `mermaid::DiagramElement` is a custom `Element` with an id derived from the view id and the block's source start, as `table_card` ids are.
  - `request_layout` uses `window.request_measured_layout` with the available width:
    - It fetches or computes the cached `Scene` for the key (D3).
    - `scale = min(1, available_w / scene_w)` and `size = (scene_w * scale, scene_h * scale)`.
    - Layout happens here on a cache miss, because `TextMeasurer` needs the `Window`.
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
  - transparent background (the card supplies it)
  - text and foreground `p.text`
  - lines `p.muted`
  - node fill `p.card`
  - node border `p.border`
  - accents for clusters and notes from `p.accent_dim`.
- **When it is built:** at paint from `crate::theme::palette(cx)`, so the dark/light switch only recolours.
- **Fonts:** `Fonts { sans: "IBM Plex Sans", mono: <the theme's mono family> }`.
- **Why `theme_only`:** authors' colours are tuned for Mermaid's default white page and are often unreadable in dark mode. The spec requires ignoring them.

### D5. Card, toggle and fallback

- **Card:** `mermaid_card` reuses the code-block frame and header styling from `md::styled`, factored out so both share it:
  - the label `mermaid`
  - a two-state segmented toggle (Diagram / Source)
  - `CopyButton` with the body.
- **Source mode:** shows the body through `code::HighlightedLines` with no language. That matches the plain code look without nesting a `TextView`.
- **Toggle state:** kept in a `Kept`-owned `HashSet<String>` of block ids shown as source, so it survives rows being rebuilt and lasts while the session is open. `Kept::clear` resets it on session switch.
- **Fallback:**
  - `parse` or `layout` errors, `Diagram::diagram_type` not supported (`limu::supports`) and limit errors all render the Source mode, plus a single muted line `Diagram not drawn: <reason>`.
  - The reason comes from limu's `ErrorCode`, mapped to short text: syntax error, unsupported diagram type, too large.
  - The toggle is hidden in this state.

### D6. Spike first: dependency and target check

Before building anything, add the dependencies and prove:
1. `cargo tree -d` shows a single `gpui-pre`.
2. Native and `wasm32-unknown-unknown` builds pass.
3. limu-gpui paints a sample in a GPUI test window.

The spike also confirms the exact `Theme` fields and `Scene` size accessors that D2 and D4 rely on.

If the spike fails on the GPUI version or the wasm target, the fallback is:
- keep `limu` for parse and layout, measured with a `TextMeasurer` of our own (the trait is tiny);
- port the painter (about 800 lines, MIT) into `src/ui/mermaid.rs` against gpui-kit's re-exports.

This keeps the same design without the crate.

## Risks / Trade-offs

- [limu is young and its API may change in any 0.x release] → Pin `~0.2`, keep all limu calls inside `src/ui/mermaid.rs`, and cover them with tests that fail on API drift.
- [Rotated labels and halos use usvg with system fonts, which are absent on the wasm build] → Rotated labels may be missing on web. They are rare (axis titles, swimlane names). Note this in the knowledge docs, and if it is unacceptable, ship with the IBM Plex font bytes registered with usvg through limu-gpui's asset path.
- [gpui-kit later enables `gpui-fast`, making limu-gpui link a different GPUI] → The spike's `cargo tree -d` check becomes a CI-visible failure (two GPUIs cause type errors). The fallback in D6 applies.
- [A very wide diagram scaled to fit becomes too small to read] → The Source toggle stays available, and zoom or a separate viewer is a possible follow-up change.
- [Synchronous layout of a huge diagram stalls a frame] → limu's limits bound it, and a timing test with a 400-edge flowchart must stay under 16 ms in release. Otherwise move layout to `cx.background_spawn` with a provisional height, accepting one re-measure.
- [`add-session-find` reconstructs rendered text from the Markdown source] → A diagram's text is its body (D1, `.text(body)`), as for a native code block without its fence. Its corpus test gains a mermaid fixture once both changes land.
