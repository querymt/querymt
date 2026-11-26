# Tasks

This change sets `skip_specs: true` (test-only tooling plus one element id), so no task covers spec scenarios. Commands run from the repository root. Each finding is recorded under its task, with the date, the machine and the exact command.

The film run command used below, called **FILM** for short:

```bash
QMT_FILM_SCENE=<scene> cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(=film::film_scene)' --no-capture
```

## 1. Module split and scene parsing

- [ ] 1.1 Move `src/film.rs` to `src/film/mod.rs` and create empty `scene.rs`, `agent.rs`, `track.rs` and `timing.rs` (design D1). No behaviour change yet.
  - Verify: `cargo check -p querymt-gpui --bins --tests` passes, and `cargo nextest list -p querymt-gpui --bins --run-ignored only` still lists the four spike tests.
- [ ] 1.2 Add `toml = { workspace = true }` to `[dev-dependencies]` in `crates/querymt-gpui/Cargo.toml`, and the serde types for scene `version = 1` in `scene.rs` (design D2):
  - settings, `world` and steps;
  - one action per step, `caption`, `window`, `at`/`after`;
  - id and `{ at = [x, y] }` targets.
  - Verify: `cargo check -p querymt-gpui --bins --tests` passes, and the `Cargo.lock` diff is empty.
- [ ] 1.3 Validation and timing resolution in `scene.rs`:
  - absolute times from `at`/`after`;
  - `type` and `stream` durations from `cps`/`wps`;
  - frame rounding;
  - the D2 rules: monotonic times, exactly one action per step, `end` present and last, a seeded world without agent steps, panel steps only after `quick_open`.

  Errors name the step index and the key.
  - Verify: unit tests in `film/scene.rs` cover one valid scene, each rule, and the frame rounding of `type`/`stream` durations, and pass under `cargo t-gpui -E 'test(/film::scene/)'`. The tests are not ignored, so CI runs them.

## 2. World, fake agent and input

- [ ] 2.1 Build the world from `[world]`:
  - fixture: `schedule_ui_fixture` plus `rpc_for_test`;
  - sessions via `runtime_mut_for_test(..).sessions`, with `sessions_loaded = true`;
  - models via `launch::add_models_for_test`, with `models_loaded`;
  - agent title, `prefs.cwd`, and the optional open session via `install_main_for_test` + `show_session_for_test`;
  - otherwise the Start page;
  - `kind = "agent"` keeps the spike's `websocket_fixture` seed;
  - theme and accent from the scene.
  - Verify: `crates/querymt-gpui/film/scenes/smoke.toml` (two sessions, one model, no steps but `end` at 0.5 s) films with **FILM**, and `smoke-000.png` shows both sessions in the sidebar and the model in the pill.
- [ ] 2.2 Fake agent in `agent.rs` (design D3):
  - drain the request channel every frame and auto-reply;
  - hold `session/prompt` as the open turn;
  - notification builders for `stream`, `tool_call`, `state`, `end_turn` and `notify`.
  - Verify: unit tests in `film/agent.rs` show which requests are answered and with what (held prompt, `session/new` id), and the JSON shape of each builder, and pass under `cargo t-gpui -E 'test(/film::agent/)'`.
- [ ] 2.3 Add `.id("composer-input")` to the composer input in `src/ui/session_view.rs` (design D7). Then check `window.try_find("composer-input")` on the film world's main window.
  - If it is `None`, apply the D7 fallback (the gpui-kit registration used by `kit::icon_button`, from the gpui-base source) and record what was needed.
  - Verify:
    - a unit test in `film/mod.rs` asserts that `try_find("composer-input")` is `Some` after one frame; this test is not ignored because it needs no ffmpeg, so CI runs it;
    - `cargo t-gpui` passes in full.
- [ ] 2.4 Input steps in the frame loop (design D4):
  - `type` (one `input` chunk per frame, sized from `cps`), `press`, `click`, `hover`, `scroll`;
  - point targets through `Window::dispatch_event`.
  - Extend `smoke.toml`: click `composer-input`, type a prompt, press `enter`, stream a short reply, then `end_turn`.
  - Verify: **FILM** on `smoke.toml` passes, and the last still shows the user prompt (sent through the composer), the reply, and the idle composer.

## 3. Timing file

- [ ] 3.1 `timing.rs` (design D6):
  - event log entries with `rect` and `pct`, from `find(id).bounds()` or `at`;
  - marks, captions and tracks;
  - writers for `<name>.film.json` and `film.js` (`window.QMT_FILM = …;`).
  - Verify: unit tests in `film/timing.rs` cover the `pct` maths, caption spans and the `film.js` wrapper, and pass under `cargo t-gpui -E 'test(/film::timing/)'`.
- [ ] 3.2 Wire the event log into the loop, and write the outputs to `$QMT_FILM_OUT/<name>/` (default `target/film/<name>/`): `main.mp4`, `<name>.film.json`, `film.js` and `still-<frame>.png` for `stills`.
  - Verify:
    - **FILM** on `smoke.toml` passes;
    - in `smoke.film.json`, the `click` event's `rect` matches the composer in the still (checked by eye and recorded), and the `type` event has `text` and `until`;
    - `node -e "require('fs'); global.window={}; require('./target/film/smoke/film.js'); console.log(Object.keys(window.QMT_FILM))"` prints the top-level keys.

## 4. Quick panel

- [ ] 4.1 Add `QuickHost::install` to the world when any step uses the panel, plus `quick_open`/`quick_close` (design D4). `Film` holds a list of windows; the panel's composer gets focus on open.
  - Verify: a `smoke-panel.toml` scene (open the panel, type, `enter`, `stream`, `end_turn`, close) films with **FILM**, and the request log printed by the test shows `session/new` then `session/prompt` from the panel.
- [ ] 4.2 Panel track (design D5):
  - fixed canvas: the panel width × maximum height (record the values and where they come from);
  - bottom-centre placement and the rounded alpha mask at `chrome::RADIUS` × scale;
  - VP9 alpha `panel.webm`, with `$QMT_FILM_PANEL_CODEC=prores` → `panel.mov`;
  - per-frame bounds and `visible` ranges in the timing file;
  - a transparent canvas while the panel is closed.
  - Verify:
    - `ffprobe -v error -show_entries stream=codec_name,pix_fmt:stream_tags=alpha_mode -of default=nw=1 target/film/smoke-panel/panel.webm` reports `vp9`, `yuva420p` (or VP9 with `alpha_mode=1`) and the same frame count as `main.mp4`;
    - a still extracted with `ffmpeg -c:v libvpx-vp9 -i panel.webm -frames:v 1 -vf select=eq(n\,<frame>) out.png` has transparent corners (alpha 0 at pixel 0,0);
    - unit tests for the mask (corner alpha 0, centre 255, edge in between) pass in `film/track.rs`.
- [ ] 4.3 `panel = "composite"`: alpha-blend the masked panel and a soft shadow onto the main frame at the scene anchor (design D5), with no `panel.webm`.
  - Verify: **FILM** on `smoke-panel.toml` with `panel = "composite"` writes only `main.mp4`, and a recorded still shows the panel over the main window with rounded corners and no black corners.
- [ ] 4.4 Record the timings for `smoke-panel.toml` in both modes: total wall time, ms per frame for capture, VP9 encode wait.
  - Verify: the numbers are recorded below this task.

## 5. Scenes, cleanup and docs

- [ ] 5.1 Write `crates/querymt-gpui/film/scenes/prompt-and-reply.toml`, reproducing the spike's clip (same prompt, reply and timing, fixture world now with sessions and a model). Remove `film_prompt_and_streamed_reply`, `_seeded`, `probe_headless_input` and `probe_seeded_from_agent`.
  - Verify: **FILM** on it passes, and its frame-90 still matches the spike's composition (prompt and partly streamed reply), apart from the now-populated sidebar and model pill.
- [ ] 5.2 Rewrite "Filming demo clips" in `crates/querymt-gpui/knowledge/development.md`:
  - the **FILM** command and `QMT_FILM_SCENE`, `QMT_FILM_OUT`, `QMT_FILM_AGENT`, `QMT_FILM_PANEL_CODEC`;
  - the scene format (D2) with one full example;
  - the fake agent's replies (D3);
  - the outputs and timing file (D6);
  - panel modes and their limits (no native material or edge light, opaque chrome);
  - the composer id and the target rules.

  Remove "The quick panel is not filmable". Add `crates/querymt-gpui/src/film` and `crates/querymt-gpui/film` to the note's `knowledge_paths`.
  - Verify: the documented command runs as written on `prompt-and-reply.toml`, and `cargo run -p querymt-agent --example repository_knowledge -- --root . check` reports nothing new for this note (the existing `agent-runtime` warnings are listed as pre-existing).

## 6. Package checks

- [ ] 6.1 `cargo check -p querymt-gpui --bins --tests` passes.
- [ ] 6.2 `cargo t-gpui` passes. Record the pass and skip counts; the film tests are skipped, and the scene, agent and timing unit tests plus the composer-id test run.
- [ ] 6.3 `cargo fmt -p querymt-gpui -- --check` passes.
- [ ] 6.4 `cargo cl-gpui` passes.
- [ ] 6.5 `RUSTC_BOOTSTRAP=1 cargo check -p querymt-gpui --target wasm32-unknown-unknown` passes. Only the composer id reaches the web build.
- [ ] 6.6 `openspec validate add-gpui-film-scenes --strict` passes.

## Workflow follow-up

- Apply `add-gpui-demo-videos`, which moves clip scenes to `demos/` and renders them with HyperFrames.
- Archive this change after review.
