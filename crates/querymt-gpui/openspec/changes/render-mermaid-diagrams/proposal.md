# Proposal

## Why

Agents often answer with Mermaid diagrams (flowcharts, sequence diagrams, state machines) in fenced code blocks. Today these show as raw `mermaid` source, which is hard to read. The desktop app and most chat clients draw them. A native renderer now exists that draws into GPUI directly (limu with limu-gpui), so the app can render diagrams without a web view.

## What Changes

- **Closed `mermaid` fences render as diagrams** wherever transcript Markdown is shown: user messages, replies, reasoning and Markdown tool content, in both the main window and the quick panel.
- **Diagram card:**
  - It uses the same frame and header as code blocks, with `mermaid` on the left and, on the right, a **Diagram / Source** toggle and the copy button.
  - Copy always copies the Mermaid source.
  - Source shows the fence as a plain code block.
- **Fit and theme:**
  - The diagram is drawn in the app's fonts and colours, follows light and dark mode without re-laying out, and is scaled down (never up) to fit the transcript column.
- **Streaming:**
  - While a fence is still open (the reply is streaming), it shows as a code block.
  - It becomes a diagram once the closing fence arrives, so partial diagrams never flicker.
- **Fallback:**
  - Source that does not parse, a diagram type the renderer does not support, or a diagram over the renderer's size limits shows as the code block, with a one-line note saying why.
  - It never shows an empty card.
- **Renderer choice:** `limu` (parser and layout, std-only) with `limu-gpui` (GPUI painter, measures text with GPUI's text system). Both are MIT-licensed.
  - limu-gpui depends on `gpui-pre ^0.3.11`, which unifies with gpui-kit 0.11's pinned `gpui-pre =0.3.14`.
  - The other crates considered and rejected:
    - flowmaid is GPL-3.0.
    - merman and mermaid-rs-renderer produce SVG measured with their own fonts.

**Not in this change**

- Zoom, pan or opening a diagram in a separate viewer.
- Clickable nodes and links inside diagrams.
- Rendering other diagram languages (PlantUML, Graphviz).
- Exporting diagrams as images.

## Capabilities

### New Capabilities

- `mermaid-diagrams`: how Mermaid fences in transcript Markdown are recognized, drawn, sized, themed, copied, toggled to source, and when they fall back to code.

### Modified Capabilities

None.

## Impact

- **Dependencies:**
  - `Cargo.toml` adds `limu` and `limu-gpui` (0.2.x, pinned to a minor version because their API may change in any 0.x release).
  - This brings in `lyon` 1.0 (tessellation). It also requires Rust 1.95 or later; the toolchain is 1.100.
- **Code:**
  - `src/ui/md.rs`: the block parser intercepts closed `mermaid` code nodes as a custom block, and a block renderer draws the card.
  - A new `src/ui/mermaid.rs` holds the diagram element, the layout cache, and the theme mapping from `Palette`.
- **Build:** both the native and the `wasm32-unknown-unknown` builds must compile. Rotated labels in limu-gpui go through GPUI's SVG path with system fonts, which may be missing on the web build (see design).
- **Interaction with `add-session-find`:**
  - A diagram's source is part of the view's rendered text, so find counts matches in it.
  - Find cannot paint highlights inside the custom block, and reveals the block instead.
