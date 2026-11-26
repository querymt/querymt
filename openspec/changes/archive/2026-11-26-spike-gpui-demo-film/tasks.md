# Tasks

This change sets `skip_specs: true` (test-only tooling), so no task covers spec scenarios. Each task states its own verification. Commands run from the repository root. The film test is ignored, so it never runs under `cargo t-gpui` or in CI. Each finding is written under the task that produced it (date, machine, `rustc -V`, exact command, result).

## 1. Harness skeleton

- [x] 1.1 Check the tools:
  - `cargo nextest --version` works;
  - `ffmpeg -version` works;
  - the macOS version and GPU are recorded.
  - Verify: all three are recorded below this task.
  - **Recorded 2026-11-26:** cargo-nextest 0.9.136; ffmpeg 9.1; macOS 27.0.1 (26A434), Apple M4 Pro (Metal); `rustc 1.100.0 (0ef1bc386 2026-11-09)`, host aarch64-apple-darwin.
- [x] 1.2 Add `src/film.rs` as `#[cfg(test)] mod film;` in `src/main.rs`, and make `install_fonts` in `src/main.rs` `pub(crate)` (design D1, D3).
  - The module holds `film_prompt_and_streamed_reply`, marked `#[ignore = "films a demo clip; needs ffmpeg"]`, with an empty body.
  - Verify:
    - `cargo check -p querymt-gpui --bins --tests` passes;
    - `cargo t-gpui -E 'test(/film_prompt_and_streamed_reply/)'` reports it as skipped (ignored).
  - **Recorded:** `cargo check -p querymt-gpui --bins --tests` passed. `cargo t-gpui -E 'test(/film_prompt_and_streamed_reply/)'` ran 0 tests (600 skipped, the film test among them; nextest then exits "no tests to run", as it does for any all-ignored filter). Plain `cargo t-gpui` shows it in the skipped count (see 4.2).
- [x] 1.3 Build the headless context and open the app (design D2, D3):
  - `HeadlessAppContext::with_platform`, given the platform text system, `AppAssets(gpui_kit::assets::AllAssets)` and `gpui_kit::platform::current_headless_renderer`;
  - bootstrap as in D3;
  - open a 1280x800 `QmtApp` window on the `film` session;
  - draw one frame and capture it with `capture_screenshot`;
  - write it to `$QMT_FILM_OUT` (default `target/film/`) as `frame-000.png` with the `png` crate.
  - Verify:
    - `cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(/film_prompt_and_streamed_reply/)' --no-capture` passes;
    - `target/film/frame-000.png` shows the session page (header, empty transcript, composer), at the size the image reports.
  - If the renderer fails, apply the D2 fallback (retry with `--test-threads=1`, then `VisualTestAppContext`) and record which one worked.
  - **Recorded (deviation from D2/D3, see design "Spike findings"):** the first run failed with `Mac platform not created on main thread` (`gpui-pre-macos` `MacPlatform::new`, called by `current_platform(true)` to get the text system). The two D2 fallbacks do not help: `cargo test ... -- --test-threads=1` still runs the test on a spawned thread (same panic), and `VisualTestAppContext` also builds a `MacPlatform`. Worked: build the text system directly with `gpui_apple::AppleTextSystem::new()` (no main thread needed), which needs the macOS-only dev-dependency `gpui-pre-apple = "=0.3.14"` (already in `Cargo.lock` through gpui-kit; the lock only gains an edge). `MetalHeadlessRenderer` itself works off the main thread.
  - Two more fixes were needed: `QmtApp` opens on the Start page, so a test-only `QmtApp::show_session_for_test` (`src/ui/mod.rs`) navigates to the session page and focuses the composer (as `find.rs`'s rig does); and the test window's appearance is light, so `store.prefs.dark = Some(true)` is set in memory and `apply_theme` is called.
  - Result: passes; `frame-000.png` is 2560x1600 and shows the session page (header, "No conversation yet", composer).

## 2. Scene and frame loop

- [x] 2.1 Add the hard-coded scene from design D4 and the frame loop from D5, without encoding yet:
  - steps are applied with `AgentsStore::notify_for_test("test-agent", "session/update", …)` at their frame;
  - each frame runs `advance_clock(1/30 s)`, `run_until_parked`, `render_frame` and `capture_screenshot`;
  - frame 90 is also saved as `frame-090.png`.
  - Verify:
    - the ignored test passes;
    - `frame-090.png` shows the user prompt and a partly streamed reply;
    - the printed frame count is 150.
  - **Recorded:** passes; `frames: 150`. `frame-090.png` shows the right-aligned user prompt, a heading, the intro line and the three list items, the spinner in the header and "Responding...", and the stop button; the code block has not arrived yet at that frame (frame 90 = 3.0 s). The last clip frame shows the finished reply.
- [x] 2.2 Pipe frames to `ffmpeg` as raw RGBA (D5 command), writing `target/film/prompt-and-reply.mp4`.
  - A missing `ffmpeg` fails the test with a message naming it.
  - Verify:
    - the ignored test passes;
    - `ffprobe -v error -show_entries stream=width,height,r_frame_rate,nb_frames -of csv=p=0 target/film/prompt-and-reply.mp4` reports the captured size, `30/1` and 150 frames;
    - the clip plays and shows the reply streaming in.
  - **Recorded:** `ffprobe` reports `2560,1600,30/1,150`. I checked frames 0, 90 and 149 of the clip (via the PNGs and `ffmpeg -vf select`), not a playback: frame 149 shows the whole reply, the highlighted code block, no spinner and the send button back. A human should still watch the clip once.
- [x] 2.3 Print and record the timings: total wall time, mean milliseconds per frame (captures only), and ffmpeg time.
  - Verify: the numbers from one run are recorded below this task.
  - **Recorded, 2026-11-26, Apple M4 Pro, debug test profile, `cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(/film_prompt_and_streamed_reply/)' --no-capture`:** captured size 2560x1600; 150 frames; total wall time 23.10 s; mean capture 126.0 ms/frame (the `capture_screenshot` calls only); ffmpeg wait after the last frame 102 ms. Other runs: 21.7 s / 126.5 ms (Start page, before the session page fix) and 36.5 s / 184.7 ms (while other builds ran), so expect 125-185 ms depending on load. That is below the 200 ms line in the design, so 1x scale is not needed for speed. The 5 s clip takes about 4.6x its length to render.

## 3. Findings and docs

- [x] 3.1 Compare `frame-000.png` and `frame-090.png` with a screenshot of `cargo r-gpui` at 1280x800, dark theme, orange accent.
  - Record every visual difference (fonts, icons, colours, spinner, code highlighting, window frame or material).
  - Verify: the list is recorded below this task.
  - **NOT DONE: needs a person.** I did not launch `cargo r-gpui` and screenshot it: that opens a real window on the user's machine with their real preferences and agents, and the page being compared (a streaming session) needs a live agent. What the frames alone show (so only half of the comparison): fonts (IBM Plex), Lucide icons, spacing, the dark palette with the orange accent, the code block's syntax colours and the spinner all render; there is no OS window frame or traffic-light area (a gap is left at the top left of the header); no window material or blur. Missing from the capture, to confirm against the real app: whether any colour or shadow differs, and whether the sidebar's "Loading sessions..." / the header's "Model unknown" come from the fixture only.
  - **Update:** the user confirmed that the real app (with qmtcode attached) does not show "Loading sessions…". Cause: `peer_fixture()` marks the agent connected but never sets `sessions_loaded` (`src/ui/sidebar.rs:363-371`), and has no model catalog. A seeded take (see 3.2, "Seeding from a real agent") connects to a qmtcode and shows the real session list, mesh count and a real model in the pill (`probe-seeded.png`, `prompt-and-reply-seeded-*.png`).
  - **Closed by the user:** compared the frames with the real app and found it all good (colours, shadows, header spacing, window frame). I did not do this comparison myself and have no list of differences beyond the ones recorded above (no OS window frame or traffic lights, no window material or blur). Ticked on the user's confirmation.
- [x] 3.2 Answer the design's Open Questions below this task. For headless input, try one `TestWindowExt::click` on the composer and one keystroke; record whether each works, without adding them to the scene.
  - Record the go or no-go for the follow-up change (scene file format, sidecar, `demos/` HyperFrames project).
  - Verify: every Open Question has a recorded answer.
  - **Answers:**
    - *Does `HeadlessAppContext` + `current_headless_renderer` work on macOS in this crate?* Yes, with the text-system change above (the stock `current_platform(true)` path does not work from a test thread). Linux (wgpu): not tried, no machine.
    - *Per-frame wall time at 2x:* about 126 ms for capture (up to 185 ms under load); whole loop about 150 ms per frame.
    - *Do keystrokes and clicks work on a headless window?* Keystrokes: yes. `window.press("h")` with the composer focused put "h" in the composer (`probe-input.png`). `window.press("cmd-f")` opened the find bar once the transcript had a row (it does nothing on an empty transcript), and `window.click("find-close")` closed it (`probe-find-open.png` vs `probe-find-closed.png`). So click by element id works. Clicking the composer itself does not: it has only a `debug_selector` (`composer-input`), not an element id, so `TestWindowExt::click` cannot name it, and `try_find("composer-input")` finds nothing. Follow-up would need either an id on the composer or raw `Window::dispatch_event` mouse events at `debug_bounds` positions. `platform_info().is_mac()` is true in the headless context, so use `cmd-` bindings.
    - *Are code blocks highlighted when captured?* Yes: in the last clip frame the Rust block has syntax colours (keywords, `&mut`, types, method name) and a language label. Mermaid and images were not exercised.
    - *Seeding from a real agent (asked after the first pass):* works. `launch::websocket_fixture(address)` (test-only) builds a store with one WebSocket agent and no saved state; the film calls the real `AgentsStore::connect`, allows parking (the transport thread wakes app tasks), and waits in wall time until the session list and model catalog have arrived: 1.96 s against qmtcode `nightly-451-gaf89d533` on `localhost:3030`, 250 sessions on the first page, 542 models. The scripted scene then runs unchanged on the stepped clock: `film_prompt_and_streamed_reply_seeded` passed, 150 frames, 2560x1600, 35.0 s wall time, mean capture 126.5 ms. Not reproducible: real session titles show, the catalog keeps paging in (the open-session count went from 201 to 7960 during the take), and scripted `session/update`s trigger real re-lists. The filmed session is not on the agent, so it has no sidebar row.
    - *Go or no-go:* **Go**, with three follow-up notes: (1) decide how to get the text system on macOS without a new dev-dependency or accept `gpui-pre-apple` there; (2) give the composer an element id or add a raw-mouse helper; (3) finish 3.1 against the real app before trusting visual parity. This is my recommendation from one scene on one machine; the decision is yours.
- [x] 3.3 Add a "Filming demo clips (spike)" section to `crates/querymt-gpui/knowledge/development.md`:
  - the run command;
  - the `ffmpeg` requirement;
  - `QMT_FILM_OUT`;
  - the output files;
  - the known limits from 3.1.
  - Verify:
    - the documented command runs as written;
    - `cargo run -p querymt-agent --example repository_knowledge -- --root . check` passes.
  - **Recorded:** the documented command ran as written (it is the command used for 2.3). After the seeded take was added, the stills were renamed `<take>-000.png` / `<take>-090.png` (`frame-000.png` and `frame-090.png` above are now `prompt-and-reply-000.png` and `prompt-and-reply-090.png`), and the documented filters became exact (`test(=film::…)`); `cargo nextest list` shows each matches exactly one test. The knowledge check exits 1 with six warnings, all in `crates/agent-runtime/knowledge/execution.md` (evidence changed or unverified). The same six warnings, and exit 1, appear on a clean `HEAD` worktree, none concerns `crates/querymt-gpui/knowledge/development.md`, so they are not from this change. Ticked on that basis; say so if you want it left open.

## 4. Package checks

- [x] 4.1 `cargo check -p querymt-gpui --bins --tests` passes. (Passed.)
- [x] 4.2 `cargo t-gpui` passes, with the film test skipped. Record the pass/skip counts. (595 tests run: 595 passed, 9 skipped; the skipped are ignored tests, including the four film tests `film_prompt_and_streamed_reply`, `film_prompt_and_streamed_reply_seeded`, `probe_headless_input` and `probe_seeded_from_agent`. nextest 0.9.136. Re-run after the seeded take was added.)
- [x] 4.3 `cargo fmt -p querymt-gpui -- --check` passes. (Passed.)
- [x] 4.4 `cargo cl-gpui` (clippy `--all-targets -D warnings`) passes. (Passed, no warnings.)
- [x] 4.5 `cargo check -p querymt-gpui --target wasm32-unknown-unknown` passes, so no film code leaks into the web build.
  - If the wasm target is not installed, record that and rely on `#[cfg(test)]`.
  - Ran as `RUSTC_BOOTSTRAP=1 cargo check -p querymt-gpui --target wasm32-unknown-unknown` (without it `wasm_thread` fails with E0554, as `web/build.sh` documents); passes. One warning, `tip_with_key` unused in `src/ui/kit.rs`, is not from this change. The macOS-only dev-dependency is not built for wasm.
- [x] 4.6 `openspec validate spike-gpui-demo-film --strict` passes. ("Change 'spike-gpui-demo-film' is valid".)

## Workflow follow-up

- If the spike is a go, propose the follow-up change (scene file format, sidecar of clicks and element bounds, `demos/` HyperFrames project and render script), citing the findings recorded here.
- Archive this change after review.
