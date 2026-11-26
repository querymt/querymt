# Tasks

This change sets `skip_specs: true` (test-only tooling plus one element id), so no task covers spec scenarios. Commands run from the repository root. Each finding is recorded under its task, with the date, the machine and the exact command.

The film run command used below, called **FILM** for short:

```bash
QMT_FILM_SCENE=<scene> cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(=film::film_scene)' --no-capture
```

## 1. Module split and scene parsing

- [x] 1.1 Move `src/film.rs` to `src/film/mod.rs` and create empty `scene.rs`, `agent.rs`, `track.rs` and `timing.rs` (design D1). No behaviour change yet.
  - Verify: `cargo check -p querymt-gpui --bins --tests` passes, and `cargo nextest list -p querymt-gpui --bins --run-ignored only` still lists the four spike tests.
  - Done: `cargo check -p querymt-gpui --bins --tests` passed; `cargo nextest list -p querymt-gpui --bins --run-ignored only` listed `film_prompt_and_streamed_reply_seeded`, `probe_headless_input`, `probe_seeded_from_agent` (and the non-seeded film test).
- [x] 1.2 Add `toml = { workspace = true }` to `[dev-dependencies]` in `crates/querymt-gpui/Cargo.toml`, and the serde types for scene `version = 1` in `scene.rs` (design D2):
  - settings, `world` and steps;
  - one action per step, `caption`, `window`, `at`/`after`;
  - id and `{ at = [x, y] }` targets.
  - Verify: `cargo check -p querymt-gpui --bins --tests` passes, and the `Cargo.lock` diff is empty.
  - Done: check passes. `Cargo.lock` is not literally empty: it gains one line, `"toml 1.2.2+spec-1.1.0"`, in the `querymt-gpui` dependency list. No package is added or changed (`toml` was already locked); the plan's "nothing new" holds for packages, not for this edge.
- [x] 1.3 Validation and timing resolution in `scene.rs`:
  - absolute times from `at`/`after`;
  - `type` and `stream` durations from `cps`/`wps`;
  - frame rounding;
  - the D2 rules: monotonic times, exactly one action per step, `end` present and last, a seeded world without agent steps, panel steps only after `quick_open`.

  Errors name the step index and the key.
  - Verify: unit tests in `film/scene.rs` cover one valid scene, each rule, and the frame rounding of `type`/`stream` durations, and pass under `cargo t-gpui -E 'test(/film::scene/)'`. The tests are not ignored, so CI runs them.
  - Done: `cargo t-gpui -E 'test(/film::scene/)'` 12 passed, 604 skipped. Scene additions beyond D2, needed to make it parse: `anchor = { x, bottom }` (the scene-chosen panel anchor of D5), `scroll = { target, dx, dy }`, `wait` in seconds. A step needs `at` or `after`; `at` may overlap a running `type`/`stream` (only going back before the previous step's start is refused).

## 2. World, fake agent and input

- [x] 2.1 Build the world from `[world]`:
  - fixture: `schedule_ui_fixture` plus `rpc_for_test`;
  - sessions via `runtime_mut_for_test(..).sessions`, with `sessions_loaded = true`;
  - models via `launch::add_models_for_test`, with `models_loaded`;
  - agent title, `prefs.cwd`, and the optional open session via `install_main_for_test` + `show_session_for_test`;
  - otherwise the Start page;
  - `kind = "agent"` keeps the spike's `websocket_fixture` seed;
  - theme and accent from the scene.
  - Verify: `crates/querymt-gpui/film/scenes/smoke.toml` (two sessions, one model, no steps but `end` at 0.5 s) films with **FILM**, and `smoke-000.png` shows both sessions in the sidebar and the model in the pill.
  - Done (2026-11-26, macOS arm64): `smoke.toml` has two sessions, one model and a prompt (see 2.4). FILM on it passes; `still-000.png` shows both sessions under OPEN SESSIONS and "Claude Sonnet 4.5" in the pill. Output names follow 3.2 (`still-<frame>.png`), not `smoke-000.png`.

- [x] 2.2 Fake agent in `agent.rs` (design D3):
  - drain the request channel every frame and auto-reply;
  - hold `session/prompt` as the open turn;
  - notification builders for `stream`, `tool_call`, `state`, `end_turn` and `notify`.
  - Verify: unit tests in `film/agent.rs` show which requests are answered and with what (held prompt, `session/new` id), and the JSON shape of each builder, and pass under `cargo t-gpui -E 'test(/film::agent/)'`.
  - Done: `cargo t-gpui -E 'test(/film::agent/)'` 5 passed. Addition to D3 found while filming: a real agent acknowledges a `session/prompt` by echoing it as `user_message_chunk` (with the request's `client_prompt_id` in the update's `_meta.querymt`). Without the echo the app keeps "Sending prompt…" and shows the reply above the prompt, so the fake agent now sends it (`FakeAgent::answer` returns the messages to send).

- [x] 2.3 Add `.id("composer-input")` to the composer input in `src/ui/session_view.rs` (design D7). Then check `window.try_find("composer-input")` on the film world's main window.
  - If it is `None`, apply the D7 fallback (the gpui-kit registration used by `kit::icon_button`, from the gpui-base source) and record what was needed.
  - Verify:
    - a unit test in `film/mod.rs` asserts that `try_find("composer-input")` is `Some` after one frame; this test is not ignored because it needs no ffmpeg, so CI runs it;
    - `cargo t-gpui` passes in full.
  - Done: a plain `.id("composer-input")` was not enough: the new unit test `film::the_composer_has_an_element_id` failed (`try_find` returned `None`) with only the id. D7 fallback applied: `.test_support()` after the id, the observation hook already used in `ui/md.rs`. It is `gpui_base::TestSupportExt`, re-exported at the root of gpui-kit, and returns the element unchanged without the kit's `test-support` feature (a dev-dependency only), so release and wasm builds are unaffected. The test is not ignored; on macOS it passes. Unverified on Linux CI (the film world uses `current_platform(true)` text there).
  - `cargo t-gpui` in full: 622 passed, 7 skipped.

- [x] 2.4 Input steps in the frame loop (design D4):
  - `type` (one `input` chunk per frame, sized from `cps`), `press`, `click`, `hover`, `scroll`;
  - point targets through `Window::dispatch_event`.
  - Extend `smoke.toml`: click `composer-input`, type a prompt, press `enter`, stream a short reply, then `end_turn`.
  - Verify: **FILM** on `smoke.toml` passes, and the last still shows the user prompt (sent through the composer), the reply, and the idle composer.
  - Done: `smoke.toml` clicks `composer-input`, types, presses `enter`, `state = "running"`, streams, `end_turn`; FILM passes (180 frames, 27 s). `still-090.png`: user prompt, partial reply, "Responding…". `still-179.png`: prompt, full reply, idle composer. Requests logged: `session/prompt` (from the composer), `session/list` x2.

## 3. Timing file

- [x] 3.1 `timing.rs` (design D6):
  - event log entries with `rect` and `pct`, from `find(id).bounds()` or `at`;
  - marks, captions and tracks;
  - writers for `<name>.film.json` and `film.js` (`window.QMT_FILM = …;`).
  - Verify: unit tests in `film/timing.rs` cover the `pct` maths, caption spans and the `film.js` wrapper, and pass under `cargo t-gpui -E 'test(/film::timing/)'`.
  - Done: `cargo t-gpui -E 'test(/film::timing/)'` 4 passed.

- [x] 3.2 Wire the event log into the loop, and write the outputs to `$QMT_FILM_OUT/<name>/` (default `target/film/<name>/`): `main.mp4`, `<name>.film.json`, `film.js` and `still-<frame>.png` for `stills`.
  - Verify:
    - **FILM** on `smoke.toml` passes;
    - in `smoke.film.json`, the `click` event's `rect` matches the composer in the still (checked by eye and recorded), and the `type` event has `text` and `until`;
    - `node -e "require('fs'); global.window={}; require('./target/film/smoke/film.js'); console.log(Object.keys(window.QMT_FILM))"` prints the top-level keys.
  - Done: FILM on `smoke.toml` passes. Outputs in `target/film/smoke/`: `main.mp4`, `smoke.film.json`, `film.js`, `still-000/015/090/179.png`. The `click` event's `rect` is `[359.5, 664, 814, 64]` (logical), i.e. the composer's input area inside the composer box of `still-015.png` (checked by eye); the `type` event has `text` and `until`. The node command prints `version, name, fps, frames, scale, canvas, tracks, events, marks, captions`.
  - Deviation from D6: for the panel, `rect`/`pct` and the `bounds` entries are in the panel *track's canvas* (or the main canvas in composite mode), not in raw window coordinates, because the video tool places the track by canvas.

## 4. Quick panel

- [x] 4.1 Add `QuickHost::install` to the world when any step uses the panel, plus `quick_open`/`quick_close` (design D4). `Film` holds a list of windows; the panel's composer gets focus on open.
  - Verify: a `smoke-panel.toml` scene (open the panel, type, `enter`, `stream`, `end_turn`, close) films with **FILM**, and the request log printed by the test shows `session/new` then `session/prompt` from the panel.
  - Done: `crates/querymt-gpui/film/scenes/smoke-panel.toml` (Start page behind, `quick_open`, type and `enter` in the panel, `state`, `stream`, `end_turn`, `quick_close`) films with FILM. Requests printed by the test: `session/new`, `session/set_config_option`, `session/prompt` (then `session/list` x3), so the prompt left the panel's composer through its real submit path. `QuickHost::install` runs only when a step uses the panel; the panel's composer is focused through `pane.focus_composer` after `host.show`.
  - Finding, fixed in the harness: the headless test window's `resize` changes its bounds but never tells GPUI, so `Window::viewport_size()` stayed 680x64 while the window was 680x260 and the panel painted only its bar at the top, with black below (the first composite still showed it). `Film::draw` now calls the public `Window::bounds_changed` when `viewport_size() != bounds().size`, as a platform's resize callback would. After that the panel shows its header, the transcript and the composer.
  - `type`/`press` events in the panel carry no `target`/`rect`: while the panel is only a bar (a new quick chat) its input is the bar's textarea, which has no element id that `find` resolves (registered paths: `quick-bar > Textarea > Input > input#N`). The panel track's own `bounds` give the panel position; point targets work.

- [x] 4.2 Panel track (design D5):
  - fixed canvas: the panel width × maximum height (record the values and where they come from);
  - bottom-centre placement and the rounded alpha mask at `chrome::RADIUS` × scale;
  - VP9 alpha `panel.webm`, with `$QMT_FILM_PANEL_CODEC=prores` → `panel.mov`;
  - per-frame bounds and `visible` ranges in the timing file;
  - a transparent canvas while the panel is closed.
  - Verify:
    - `ffprobe -v error -show_entries stream=codec_name,pix_fmt:stream_tags=alpha_mode -of default=nw=1 target/film/smoke-panel/panel.webm` reports `vp9`, `yuva420p` (or VP9 with `alpha_mode=1`) and the same frame count as `main.mp4`;
    - a still extracted with `ffmpeg -c:v libvpx-vp9 -i panel.webm -frames:v 1 -vf select=eq(n\,<frame>) out.png` has transparent corners (alpha 0 at pixel 0,0);
    - unit tests for the mask (corner alpha 0, centre 255, edge in between) pass in `film/track.rs`.
  - Done (2026-11-26, macOS arm64, `smoke-panel.toml`, 240 frames):
    - Canvas: `quick::panel::WIDTH` (680) x `quick::panel::MAX_HEIGHT` (760) logical, 1360x1520 at 2x. `MAX_HEIGHT` was private; it is now `pub(crate)` and, with `WIDTH`, re-exported from `quick` under `#[cfg(test)]`. No behaviour change. A panel taller than the canvas fails with a clear assertion.
    - `ffprobe -v error -show_entries stream=codec_name,pix_fmt:stream_tags=alpha_mode -of default=nw=1 target/film/smoke-panel/panel.webm`: `codec_name=vp9`, `pix_fmt=yuv420p`, `TAG:ALPHA_MODE=1` (ffprobe's native VP9 decoder drops the alpha plane, so only the tag shows it; the plan allowed this). Frame count: 240 for both `panel.webm` (`-c:v libvpx-vp9 -count_frames`) and `main.mp4`.
    - Decoding frame 150 with `ffmpeg -c:v libvpx-vp9 -i panel.webm -vf select=eq(n\,150) -fps_mode passthrough` (note: ffmpeg 9 has no `-vsync`): the canvas corners (0,0), (0,1519) and (1359,1519) have alpha 0, and (680,1220) is opaque `(33,34,39,255)`. Frame 5 (panel closed) is fully transparent.
    - `film::track` unit tests (mask corner 0, middle 255, soft edge; masking; placement; composite; codec) pass: `cargo t-gpui -E 'test(/film::/)'` 27 passed.
    - Timing file: `visible: [[15, 213]]`, `bounds: [[15, 0, 696, 680, 64], [87, 0, 500, 680, 260]]` (`[frame, x, y, w, h]` on the track canvas, bottom-centred). `$QMT_FILM_PANEL_CODEC=prores` is implemented (`panel.mov`, ProRes 4444) but was not run.

- [x] 4.3 `panel = "composite"`: alpha-blend the masked panel and a soft shadow onto the main frame at the scene anchor (design D5), with no `panel.webm`.
  - Verify: **FILM** on `smoke-panel.toml` with `panel = "composite"` writes only `main.mp4`, and a recorded still shows the panel over the main window with rounded corners and no black corners.
  - Done: `smoke-panel.toml` with `name = "smoke-panel-composite"` and `panel = "composite"` (a temporary copy in `/tmp`, not committed) wrote `main.mp4`, the timing file, `film.js` and the stills, and no `panel.webm`. `still-150.png` shows the panel over the Start page with rounded corners, a soft shadow below, its header, the streamed reply and the composer, and no black corners.

- [x] 4.4 Record the timings for `smoke-panel.toml` in both modes: total wall time, ms per frame for capture, VP9 encode wait.
  - Verify: the numbers are recorded below this task.
  - Recorded (macOS arm64, unoptimized test profile, 240 frames at 2560x1600, `smoke-panel.toml`):

    | mode | total wall time | mean capture per frame (main + panel) | `main.mp4` encode wait after last frame | `panel.webm` VP9 alpha wait after last frame |
    |---|---|---|---|---|
    | `track` | 44.8 s | 144.3 ms | 96 ms | 460 ms |
    | `composite` | 56.4 s | 143.8 ms | 104 ms | none |

    Piping to ffmpeg runs alongside rendering, so the VP9 alpha encode adds under half a second at the end. Composite costs about 12 s more over 240 frames (about 48 ms a frame) of CPU blending in an unoptimized build; ProRes was not timed.

## 5. Scenes, cleanup and docs

- [x] 5.1 Write `crates/querymt-gpui/film/scenes/prompt-and-reply.toml`, reproducing the spike's clip (same prompt, reply and timing, fixture world now with sessions and a model). Remove `film_prompt_and_streamed_reply`, `_seeded`, `probe_headless_input` and `probe_seeded_from_agent`.
  - Verify: **FILM** on it passes, and its frame-90 still matches the spike's composition (prompt and partly streamed reply), apart from the now-populated sidebar and model pill.
  - Done: `film/scenes/prompt-and-reply.toml` (the spike's prompt, reply and timing: prompt at 0.3 s, `state running` at 0.8 s, the reply from 1.0 s to 4.0 s as 58 words at 19.33 words/s = 90 frames, `end_turn` at 4.0 s, 150 frames in all; the prompt is scripted with `notify`, as in the spike). FILM passes (150 frames, 23 s). `still-090.png` shows the prompt, the heading, the intro line and the three bullets mid-stream, "Responding…", and now a populated sidebar and the model pill. `film_prompt_and_streamed_reply`, `_seeded`, `probe_headless_input` and `probe_seeded_from_agent` are gone (the module was rewritten; `cargo nextest list --run-ignored only` lists only the film tests listed in 6.2).

- [x] 5.2 Rewrite "Filming demo clips" in `crates/querymt-gpui/knowledge/development.md`:
  - the **FILM** command and `QMT_FILM_SCENE`, `QMT_FILM_OUT`, `QMT_FILM_AGENT`, `QMT_FILM_PANEL_CODEC`;
  - the scene format (D2) with one full example;
  - the fake agent's replies (D3);
  - the outputs and timing file (D6);
  - panel modes and their limits (no native material or edge light, opaque chrome);
  - the composer id and the target rules.

  Remove "The quick panel is not filmable". Add `crates/querymt-gpui/src/film` and `crates/querymt-gpui/film` to the note's `knowledge_paths`.
  - Verify: the documented command runs as written on `prompt-and-reply.toml`, and `cargo run -p querymt-agent --example repository_knowledge -- --root . check` reports nothing new for this note (the existing `agent-runtime` warnings are listed as pre-existing).
  - Done: the section is rewritten (command and four environment variables, the scene format with a full example, the fake agent, outputs and timing file, panel modes and limits, composer id and target rules); "The quick panel is not filmable" is removed. The command in the note ran as written on `prompt-and-reply.toml`.
  - `knowledge_paths` entries for the new directories matched no indexed file (`knowledge_path_unmatched`), because `.qmt/knowledge.toml` selects code files one by one. Added `crates/querymt-gpui/src/film/*.rs` and `crates/querymt-gpui/film/scenes/*.toml` to its `code.include` (the config says only the files a note explains are selected) and used those two patterns in the note's `knowledge_paths`. `cargo run -p querymt-agent --example repository_knowledge -- --root . check` then reports nothing for `crates/querymt-gpui`; the only warnings left are the pre-existing `agent-runtime/knowledge/execution.md` ones (`evidence_changed`, `evidence_unverified`, `evidence_entry_orphaned`).

## 6. Package checks

- [x] 6.1 `cargo check -p querymt-gpui --bins --tests` passes.
  - Done: passes, no warnings.
- [x] 6.2 `cargo t-gpui` passes. Record the pass and skip counts; the film tests are skipped, and the scene, agent and timing unit tests plus the composer-id test run.
  - Done: `cargo t-gpui` (nextest, 0.9.136): 622 passed, 7 skipped (the ignored tests: `film::film_scene`, `film::smoke_scene` and the five perf/telemetry tests). The 27 `film::` unit tests (12 scene, 5 agent, 4 timing, 5 track, the composer-id test) are among the 622. The ignored `film::smoke_scene` was also run on its own and passed (26 s).
- [x] 6.3 `cargo fmt -p querymt-gpui -- --check` passes.
  - Done: passes.
- [x] 6.4 `cargo cl-gpui` passes.
  - Done: `cargo cl-gpui` (`clippy -p querymt-gpui --all-targets -- -D warnings`) exits 0.
- [x] 6.5 `RUSTC_BOOTSTRAP=1 cargo check -p querymt-gpui --target wasm32-unknown-unknown` passes. Only the composer id reaches the web build.
  - Done: passes. It prints one warning, `tip_with_key` never used in `src/ui/kit.rs:498`; that file is untouched by this change, so it is not from it (not confirmed on HEAD in a separate worktree).
- [x] 6.6 `openspec validate add-gpui-film-scenes --strict` passes.
  - Done: `Change 'add-gpui-film-scenes' is valid` (`skip_specs` set, zero deltas accepted).

## Workflow follow-up

- Apply `add-gpui-demo-videos`, which moves clip scenes to `demos/` and renders them with HyperFrames.
- Archive this change after review.
