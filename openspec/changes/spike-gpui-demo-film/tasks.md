# Tasks

This change sets `skip_specs: true` (test-only tooling), so no task covers spec scenarios. Each task states its own verification. Commands run from the repository root. The film test is ignored, so it never runs under `cargo t-gpui` or in CI. Each finding is written under the task that produced it (date, machine, `rustc -V`, exact command, result).

## 1. Harness skeleton

- [ ] 1.1 Check the tools:
  - `cargo nextest --version` works;
  - `ffmpeg -version` works;
  - the macOS version and GPU are recorded.
  - Verify: all three are recorded below this task.
- [ ] 1.2 Add `src/film.rs` as `#[cfg(test)] mod film;` in `src/main.rs`, and make `install_fonts` in `src/main.rs` `pub(crate)` (design D1, D3).
  - The module holds `film_prompt_and_streamed_reply`, marked `#[ignore = "films a demo clip; needs ffmpeg"]`, with an empty body.
  - Verify:
    - `cargo check -p querymt-gpui --bins --tests` passes;
    - `cargo t-gpui -E 'test(/film_prompt_and_streamed_reply/)'` reports it as skipped (ignored).
- [ ] 1.3 Build the headless context and open the app (design D2, D3):
  - `HeadlessAppContext::with_platform`, given the platform text system, `AppAssets(gpui_kit::assets::AllAssets)` and `gpui_kit::platform::current_headless_renderer`;
  - bootstrap as in D3;
  - open a 1280x800 `QmtApp` window on the `film` session;
  - draw one frame and capture it with `capture_screenshot`;
  - write it to `$QMT_FILM_OUT` (default `target/film/`) as `frame-000.png` with the `png` crate.
  - Verify:
    - `cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(/film_prompt_and_streamed_reply/)' --no-capture` passes;
    - `target/film/frame-000.png` shows the session page (header, empty transcript, composer), at the size the image reports.
  - If the renderer fails, apply the D2 fallback (retry with `--test-threads=1`, then `VisualTestAppContext`) and record which one worked.

## 2. Scene and frame loop

- [ ] 2.1 Add the hard-coded scene from design D4 and the frame loop from D5, without encoding yet:
  - steps are applied with `AgentsStore::notify_for_test("test-agent", "session/update", …)` at their frame;
  - each frame runs `advance_clock(1/30 s)`, `run_until_parked`, `render_frame` and `capture_screenshot`;
  - frame 90 is also saved as `frame-090.png`.
  - Verify:
    - the ignored test passes;
    - `frame-090.png` shows the user prompt and a partly streamed reply;
    - the printed frame count is 150.
- [ ] 2.2 Pipe frames to `ffmpeg` as raw RGBA (D5 command), writing `target/film/prompt-and-reply.mp4`.
  - A missing `ffmpeg` fails the test with a message naming it.
  - Verify:
    - the ignored test passes;
    - `ffprobe -v error -show_entries stream=width,height,r_frame_rate,nb_frames -of csv=p=0 target/film/prompt-and-reply.mp4` reports the captured size, `30/1` and 150 frames;
    - the clip plays and shows the reply streaming in.
- [ ] 2.3 Print and record the timings: total wall time, mean milliseconds per frame (captures only), and ffmpeg time.
  - Verify: the numbers from one run are recorded below this task.

## 3. Findings and docs

- [ ] 3.1 Compare `frame-000.png` and `frame-090.png` with a screenshot of `cargo r-gpui` at 1280x800, dark theme, orange accent.
  - Record every visual difference (fonts, icons, colours, spinner, code highlighting, window frame or material).
  - Verify: the list is recorded below this task.
- [ ] 3.2 Answer the design's Open Questions below this task. For headless input, try one `TestWindowExt::click` on the composer and one keystroke; record whether each works, without adding them to the scene.
  - Record the go or no-go for the follow-up change (scene file format, sidecar, `demos/` HyperFrames project).
  - Verify: every Open Question has a recorded answer.
- [ ] 3.3 Add a "Filming demo clips (spike)" section to `crates/querymt-gpui/knowledge/development.md`:
  - the run command;
  - the `ffmpeg` requirement;
  - `QMT_FILM_OUT`;
  - the output files;
  - the known limits from 3.1.
  - Verify:
    - the documented command runs as written;
    - `cargo run -p querymt-agent --example repository_knowledge -- --root . check` passes.

## 4. Package checks

- [ ] 4.1 `cargo check -p querymt-gpui --bins --tests` passes.
- [ ] 4.2 `cargo t-gpui` passes, with the film test skipped. Record the pass/skip counts.
- [ ] 4.3 `cargo fmt -p querymt-gpui -- --check` passes.
- [ ] 4.4 `cargo cl-gpui` (clippy `--all-targets -D warnings`) passes.
- [ ] 4.5 `cargo check -p querymt-gpui --target wasm32-unknown-unknown` passes, so no film code leaks into the web build.
  - If the wasm target is not installed, record that and rely on `#[cfg(test)]`.
- [ ] 4.6 `openspec validate spike-gpui-demo-film --strict` passes.

## Workflow follow-up

- If the spike is a go, propose the follow-up change (scene file format, sidecar of clicks and element bounds, `demos/` HyperFrames project and render script), citing the findings recorded here.
- Archive this change after review.
