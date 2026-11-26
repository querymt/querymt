# Proposal

## Why

We want short feature clips of QueryMT Desktop that we can remake whenever the UI changes. Screen recording means manual retakes after every UI change, so the clips go stale. GPUI ships an offscreen renderer behind `test-support` (`HeadlessAppContext` with the platform's headless renderer, a fake clock, and `capture_screenshot`). If that can render the real app frame by frame, a clip becomes a scripted scene we re-render on demand, and HyperFrames can compose it into the final video.

Some things are unproven: does the headless renderer work from a test in this crate, do frames match the real app, how fast is it per frame, and do background work and animations settle on a stepped clock. This change is a time-boxed spike to find out with one hard-coded scene before a scene format, a sidecar or a HyperFrames project is designed.

## What Changes

- **Film harness (test-only):** a new `#[cfg(test)]` module in `querymt-gpui`, plus one `#[ignore]`d test that films a single hard-coded scene:
  - the real `QmtApp` in a headless window at 1280x800 logical size (2x scale);
  - a user prompt, then an agent reply streamed in chunks through the store's normal notification path;
  - 5 seconds at 30 fps on a stepped fake clock;
  - each frame captured and piped to `ffmpeg`, producing an MP4.
- **Output location:** `target/film/<scene>.mp4` by default, overridable with an env var. A first frame and a mid-stream frame are also written as PNGs for review.
- **Findings:** recorded in `tasks.md` and in the design's Open Questions resolution:
  - whether it runs on macOS (Metal), and on Linux (wgpu) if a machine is available;
  - wall time per frame;
  - visual differences from the running app (fonts, icons, theme, blur or material effects);
  - whether Mermaid, code highlighting and images settle before capture;
  - a go or no-go for the follow-up change.
- **Docs:** one short section in `crates/querymt-gpui/knowledge/development.md` on how to run the film test, what it needs (`ffmpeg` on `PATH`) and its limits.

No product behaviour changes. No public Rust API, config, hook, middleware, ACP or provider wire shape, Extism contract, session DB schema, Python binding or typeshare type changes. Nothing ships in release builds, because all code is `#[cfg(test)]`.

## Non-goals

- A scene file format (TOML or JSON), a scene library, or loading scenes at run time.
- A sidecar of click positions and element bounds for cursor and zoom overlays.
- A HyperFrames project (`demos/`), compositions, or a render script.
- Recording real agent sessions into scenes (an RPC recorder).
- Filming the quick panel (a native macOS panel, not rendered by GPUI's offscreen path).
- CI: the film test stays ignored and is never run in `test-gpui.yml`.
- Pixel-exact golden screenshots or visual regression tests.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. This is test-only developer tooling with no behaviour change, so the change sets `skip_specs: true`.

## Impact

- **Packages:** `querymt-gpui` only (bin target, `#[cfg(test)]` code). No feature flags change code. `querymt-agent-session-core` is used through its existing `test-support` dev-dependency.
- **Dependencies:** none new expected. Frames are passed to `ffmpeg` as raw RGBA over stdin, and preview PNGs use the `png` crate the app already depends on. `gpui-kit`'s `test-support` (already a dev-dependency) brings `HeadlessAppContext` and `gpui_platform::current_headless_renderer`.
- **Tools:** `ffmpeg` on `PATH`, needed only by the ignored film test.
- **Repository files:** a new `src/film.rs` (test module), `src/main.rs` (one `#[cfg(test)] mod film;` line), `knowledge/development.md`.
- **Special builds:** none. wasm32 and release builds compile no film code.
- **CI:** unchanged. `cargo t-gpui` skips the ignored test.
- **querymt-desktop:** no UI follow-up.
