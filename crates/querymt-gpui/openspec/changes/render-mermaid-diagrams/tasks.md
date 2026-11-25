# Tasks

Verification commands:
- `cargo nextest run --package querymt-gpui --bins --locked`
- `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`

## 1. Spike: dependencies and targets (design D6)

- [ ] 1.1 Add `limu = "~0.2"` and `limu-gpui = "~0.2"` to `Cargo.toml`. Verify:
  - `cargo tree -d -p querymt-gpui` lists no second `gpui-pre`;
  - `cargo check --locked` passes;
  - the wasm build command passes.

  If either target fails, record why here and switch to the D6 fallback before continuing.
- [ ] 1.2 Add a `#[gpui_kit::test]` in a new `src/ui/mermaid.rs` that parses and lays out `flowchart LR; a --> b` with `limu_gpui::TextMeasurer`, and paints it in a test window. Verify: the scene has two nodes and one edge, painting does not panic, and the `Theme` fields and `Scene` size accessors D2 and D4 rely on are confirmed (note any differences in the design).

## 2. Diagram element, cache and look

- [ ] 2.1 Implement the layout cache (D3) and `DiagramElement` (D2): measured layout from the cached scene, scale-to-fit without upscaling, and paint with `limu_gpui::paint`. Verify:
  - GPUI tests for "Wide diagram" (half scale, height halves) and "Small diagram" (natural size);
  - a test that a second layout with the same key does not re-run limu (cache hit counter);
  - a resize test that the scale changes and the cache is not missed.
- [ ] 2.2 Implement `look(&Palette)` with `theme_only` and the app fonts (D4). Verify: a unit test that the light and dark palettes produce different `Look`s with the mapped colours, and a GPUI test for "Switch to dark mode" (size unchanged, no new layout).
- [ ] 2.3 Add a timing test: a 400-edge flowchart laid out in release under 16 ms, or record the decision to move layout to the background (design Risks). Verify: the number is recorded here.

## 3. Markdown integration

- [ ] 3.1 Intercept closed `mermaid` fences in `md::styled`'s block parser as `MERMAID_BLOCK` with `.text(body)` (D1), including the closing-fence check for backtick and tilde fences of any length. Verify:
  - unit tests on the parser hook for a closed fence, an unclosed fence (not intercepted), `Mermaid` uppercase, and a `rust` fence (not intercepted);
  - the spec scenarios "Flowchart in a reply" and "Other languages untouched" pass as GPUI tests.
- [ ] 3.2 Factor the code-block header out of `md::styled` and build `mermaid_card` with the label, the Diagram/Source toggle and `CopyButton` (D5); keep the toggle state in `Kept`. Verify: GPUI tests for "Copy the source", "Toggle to source" and "Scroll away and back"; the existing code-block header tests pass unchanged.
- [ ] 3.3 Implement the fallback (D5): the source code block plus a `Diagram not drawn: <reason>` note for parse errors, unsupported types and limit errors, with the toggle hidden. Verify: GPUI tests for "Syntax error" and "Unsupported type", and that copy still copies the body.
- [ ] 3.4 Streaming: verify with a GPUI test for "Diagram streams in". Push chunks into a reply: a code block while the fence is open, a diagram once it closes. Also verify no row height jump at the switch beyond the block's own size change.
- [ ] 3.5 Transcript stability: verify with a GPUI test for "Scrolling past a diagram". Scroll up past a large diagram row and assert that the anchor row's position does not jump (reusing the scroll-anchor harness at `session_view.rs:6244`).

## 4. Docs and integration checks

- [ ] 4.1 Document Mermaid rendering in `knowledge/ui.md`: the interception rule, the element and cache, theming, the fallback, and the wasm rotated-label limitation. Verify: the section names the files and the design decisions.
- [ ] 4.2 Run the verification commands and `openspec validate render-mermaid-diagrams --type change --strict --no-interactive`. Verify: all pass; record the counts here.
- [ ] 4.3 By hand on macOS:
  - a flowchart, a sequence diagram, a state diagram and a class diagram from a real agent reply, in light and dark mode;
  - a window resize;
  - the quick panel;
  - a syntax error.

  Then check the wasm build in a browser for the rotated-label limitation. Verify: each is noted here as pass or fail.

## Workflow follow-up

- If `add-session-find` lands first or alongside, add a mermaid fixture to its rendered-text corpus test (its task 1.2).
- Archive after review.
