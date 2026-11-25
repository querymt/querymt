# Tasks

Verification commands:
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`

## 1. Spike: dependencies and targets (design D6)

- [x] 1.1 Add `limu = "~0.2"` and `limu-gpui = "~0.2"` to `Cargo.toml`. Verify:
  - `cargo tree -d -p querymt-gpui` lists no second `gpui-pre`;
  - `cargo check --locked` passes;
  - the wasm build command passes.

  If either target fails, record why here and switch to the D6 fallback before continuing.

  Result: `limu-gpui` 0.2.1 fails to compile against gpui-pre 0.3.14 (gpui-kit 0.11's pin). 0.3.14 replaced lyon's `FillOptions::non_zero()` and `StrokeOptions::with_line_width` with its own types (`gpui::FillOptions { fill_rule }`, `StrokeOptions { line_width, .. }`), and no newer limu-gpui exists. The D6 fallback applies: `limu` 0.2 only in `Cargo.toml`, and the painter is ported (MIT, attributed) into `src/ui/mermaid/paint.rs` and `svg.rs`. After the port, `cargo tree -d` shows a single gpui-pre 0.3.14, `cargo check --locked` and the wasm build pass.
- [x] 1.2 Add a `#[gpui_kit::test]` in a new `src/ui/mermaid.rs` that parses and lays out `flowchart LR; a --> b` with the ported `TextMeasurer`, and paints it in a test window. Verify: the scene has two nodes and one edge, painting does not panic, and the `Theme` fields and `Scene` size accessors D2 and D4 rely on are confirmed (note any differences in the design).

  Result: `flowchart_lays_out_and_paints` passes. `Scene::width`/`height` are public fields and `limu::scene::style::Theme` has `bg`, `fg`, `muted`, `line`, `surface`, `surface_alt`, `border`, `accent`, `critical`, `series[8]`, `radius`, `stroke_width`. Differences are noted in design D4 (the `bg` token is also the backing of edge labels, so it is opaque) and D2 (layout runs in the card's render, not in the measure callback).

## 2. Diagram element, cache and look

- [x] 2.1 Implement the layout cache (D3) and `DiagramElement` (D2): measured layout from the cached scene, scale-to-fit without upscaling, and paint with `limu_gpui::paint`. Verify:
  - GPUI tests for "Wide diagram" (half scale, height halves) and "Small diagram" (natural size);
  - a test that a second layout with the same key does not re-run limu (cache hit counter);
  - a resize test that the scale changes and the cache is not missed.
- [x] 2.2 Implement `look(&Palette)` with `theme_only` and the app fonts (D4). Verify: a unit test that the light and dark palettes produce different `Look`s with the mapped colours, and a GPUI test for "Switch to dark mode" (size unchanged, no new layout).
- [x] 2.3 Add a timing test: a 400-edge flowchart laid out in release under 16 ms, or record the decision to move layout to the background (design Risks). Verify: the number is recorded here.

  Result: 2.1 ms in an optimized build (opt-level 2, no LTO; the test asserts under 16 ms outside debug builds) and 8.1 ms in a debug build, with the test platform's text system. Layout stays synchronous.
- [x] 2.4 Fix the flex minimum width (D2): the element sets `min_size.width = 0` and answers a min-content measure with 0, so a wide diagram shrinks inside the card's flex row instead of being clipped on both sides. Verify: `wide_diagram_fits_the_card` lays a 24-node flowchart out in the real card and fails without the fix (`3110px overflows 574px`).

## 3. Markdown integration

- [x] 3.1 Intercept closed `mermaid` fences in `md::styled`'s block parser as `MERMAID_BLOCK` with `.text(body)` (D1), including the closing-fence check for backtick and tilde fences of any length. Verify:
  - unit tests on the parser hook for a closed fence, an unclosed fence (not intercepted), `Mermaid` uppercase, and a `rust` fence (not intercepted);
  - the spec scenarios "Flowchart in a reply" and "Other languages untouched" pass as GPUI tests.
- [x] 3.2 Factor the code-block header out of `md::styled` and build `mermaid_card` with the label, the Diagram/Source toggle and `CopyButton` (D5); keep the toggle state in `Kept`. Verify: GPUI tests for "Copy the source", "Toggle to source" and "Scroll away and back"; the existing code-block header tests pass unchanged.
- [x] 3.3 Implement the fallback (D5): the source code block plus a `Diagram not drawn: <reason>` note for parse errors, unsupported types and limit errors, with the toggle hidden. Verify: GPUI tests for "Syntax error" and "Unsupported type", and that copy still copies the body.
- [x] 3.4 Streaming: verify with a GPUI test for "Diagram streams in". Push chunks into a reply: a code block while the fence is open, a diagram once it closes. Also verify no row height jump at the switch beyond the block's own size change.
- [x] 3.5 Transcript stability: verify with a GPUI test for "Scrolling past a diagram". Scroll up past a large diagram row and assert that the anchor row's position does not jump (reusing the scroll-anchor harness at `session_view.rs:6244`).

## 4. Docs and integration checks

- [x] 4.1 Document Mermaid rendering in `knowledge/ui.md`: the interception rule, the element and cache, theming, the fallback, and the wasm rotated-label limitation. Verify: the section names the files and the design decisions.
- [x] 4.2 Run the verification commands and `openspec validate render-mermaid-diagrams --type change --strict --no-interactive`. Verify: all pass; record the counts here.

  Result: nextest 507 passed, 5 skipped (the 5 were already skipped); clippy `-D warnings` clean; `cargo fmt --check` clean; wasm32 build passes; `openspec validate` reports the change valid.
- [x] 4.3 By hand on macOS:
  - a flowchart, a sequence diagram, a state diagram and a class diagram from a real agent reply, in light and dark mode;
  - a window resize;
  - the quick panel;
  - a syntax error;
  - zoom and pan (section 5) with a trackpad (pinch, Cmd+two-finger scroll, plain scroll passing to the transcript) and a notched mouse wheel (Cmd+wheel, plain wheel), and dragging the 400-edge flowchart zoomed in, without stutter.

  Then check the wasm build in a browser for the rotated-label limitation. Verify: each is noted here as pass or fail.

## 5. Zoom and pan (design D7)

- [x] 5.1 In `SmoothScroll::install`, let wheel events with the platform or control modifier through without swallowing or smoothing them. Verify: a test that a Cmd+wheel notch is not converted into injected pixel steps, and the existing smooth-scroll tests pass.
- [x] 5.2 Add the viewport math to `src/ui/mermaid.rs` as pure functions: fitted scale with the 480px cap, zoom about a point, the zoom range, and offset clamping with centring. Verify: unit tests for the "Tall diagram" scale, a fixed point under zoom, both zoom limits, and clamping on both axes.
- [x] 5.3 Make `DiagramElement` interactive (D7): a hitbox, the bubble-phase modifier-wheel zoom with `stop_propagation`, pinch zoom, drag to pan with `GlobalState::suppress_text_selection`, double-click to Fit, the Grab/Grabbing cursor, and painting at `fit * zoom` and the offset. The element's size stays the fitted size.
- [x] 5.4 Replace the interim Fit/100% toggle and scroller in `MermaidCard` with the zoom controls (`−`, percentage, `+`, `Fit`, with tooltips naming the gestures), and replace `CardModes.actual` with `views` (D5). Remove `diagram_actual`, `natural_size` and `ACTUAL_MAX_HEIGHT`. Verify: the controls are disabled at the limits and hidden in Source mode and in the fallback.
- [x] 5.5 GPUI tests in the real card inside a transcript `list`, one per new spec scenario: "Modifier-wheel zoom", "Zooming does not scroll the transcript" (the list offset is unchanged after Cmd+wheel and pinch), "Plain scroll passes through" (the list scrolls, the viewport is unchanged), "Pinch zoom", "Drag to pan" (no selected text, clamped), "Back to Fit" (button and double-click), "Zoom limits", "Zoom keeps the row height", "Zoomed diagram scrolls away and back" and "Tall diagram". The existing fit, cache and theme-switch tests pass.
- [x] 5.6 Update the Mermaid section of `knowledge/ui.md`: the height cap, the viewport store, wheel ownership, the smooth-scroll bypass, and the removal of Fit/100%.
- [x] 5.7 Run the verification commands and `openspec validate render-mermaid-diagrams --type change --strict --no-interactive`. Verify: all pass; record the counts here.

  Result: nextest 543 passed, 5 skipped (the 5 were already skipped; 507 before section 5, with 36 new tests: viewport maths, smooth-scroll bypass, and one per new spec scenario); clippy `-D warnings` clean; `cargo fmt --check` clean; wasm32 build passes; `openspec validate` reports the change valid.

## Workflow follow-up

- If `add-session-find` lands first or alongside, add a mermaid fixture to its rendered-text corpus test (its task 1.2).
- Archive after review.
