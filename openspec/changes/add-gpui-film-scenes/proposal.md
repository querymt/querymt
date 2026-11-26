# Proposal

## Why

The archived spike `2026-11-26-spike-gpui-demo-film` showed that the real QueryMT Desktop app can be filmed offscreen, frame by frame, on a stepped clock: 2560x1600, 30 fps, about 126 ms per frame. It filmed one clip, hard-coded in `src/film.rs`.

We want many short feature clips that we remake whenever the UI changes. That needs four things the spike lacks:

- clips described as data, so a new or changed clip needs no Rust edit and no recompile;
- real input (typing, clicks) and a fake agent that answers the app's requests;
- a record of where and when things happened, so a video tool can place a cursor, zooms and captions;
- the quick panel on film.

This change builds that harness. The companion change `add-gpui-demo-videos` builds the HyperFrames project that turns its output into finished videos.

## What Changes

- **Scene files.** A clip is a TOML file:
  - settings: canvas size, fps, theme, accent;
  - a starting state: fake sessions, models and agent title, or a seed from a running qmtcode as in the spike;
  - a timed list of steps.

  A single ignored test, `film::film_scene`, reads the file named by `$QMT_FILM_SCENE`. The test binary is built once; editing a scene and rerunning needs no recompile. The spike's hard-coded clip becomes the first scene file.
- **Steps:**
  - input: `wait`, `type` (text at a typing speed), `press` (a key), `click` and `hover` (by element id or debug selector), `scroll`;
  - quick panel: `quick_open` and `quick_close`;
  - agent: `stream` (reply text at a word rate), `tool_call`, `state`, `end_turn`, and a raw `notify` (any notification JSON);
  - `mark` (record an element's bounds under a name);
  - `caption` on any step.
- **Fake agent.** It answers the app's requests the way the quick-panel test rig does:
  - `session/new` gets a scene-chosen session id;
  - `session/prompt` is held open until `end_turn`;
  - everything else gets `{}`.

  So a prompt typed into the composer and sent with Enter goes through the app's real send path.
- **Timing file** `<clip>.film.json`, written next to the footage:
  - fps, frame count and canvas size;
  - one track per window, with its file and its bounds over time;
  - every input event: frame, time, window, target, and rectangle in logical pixels and in % of the canvas;
  - named marks and captions.
- **Quick panel on film:**
  - the film world installs `QuickHost`, and `Film` holds several windows;
  - the panel is captured as its own track, on a fixed-size, bottom-anchored canvas, with a rounded alpha mask at `chrome::RADIUS`. The offscreen Metal renderer is built opaque, so the corners otherwise come out black;
  - it is encoded as a WebM with alpha (VP9, `yuva420p`);
  - the scene setting `panel = "composite"` instead draws it over the main window into one MP4.
- **Outputs:** everything goes to `$QMT_FILM_OUT/<clip>/` (default `target/film/<clip>/`):
  - `main.mp4`, plus `panel.webm` when the scene uses the panel;
  - `<clip>.film.json`, and the same data as `film.js`, which sets `window.QMT_FILM`;
  - `still-*.png` for frames the scene asks for.
- **Product change (small):** the composer's input element (`src/ui/session_view.rs`, today only `debug_selector("composer-input")`) gets the element id `composer-input`, so it can be clicked by id. No visible or behavioural change.
- **Docs:** the "Filming demo clips" section of `crates/querymt-gpui/knowledge/development.md` is rewritten. That includes removing the wrong claim that the quick panel cannot be filmed.

No public Rust API, config, hook, middleware, ACP or provider wire shape, Extism contract, session DB schema, Python binding or typeshare type changes. Apart from the composer id, all new code is `#[cfg(test)]`.

## Non-goals

- The HyperFrames project, compositions and render script: change `add-gpui-demo-videos`.
- Recording live agent sessions into scenes.
- The native macOS panel material (Liquid Glass or visual effect), the Core Animation edge light, the window shadow, the global hotkey, the tray, `--quick` forwarding and dragging the panel. None exists on the test platform. The panel films with its opaque "Reduce transparency" chrome.
- Linux or Windows filming (still untried).
- CI: film tests stay ignored.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. This is test-only tooling. The composer id has no user-visible behaviour, so the change sets `skip_specs: true`.

## Impact

- **Packages:** `querymt-gpui` only.
- **Code:**
  - `src/film.rs` grows, or splits into `src/film/` (`scene.rs`, `runner.rs`, `agent.rs`, `track.rs`, `timing.rs`);
  - `src/ui/session_view.rs` gets the composer id;
  - small `#[cfg(test)]` helpers in `src/store` and `src/quick` where fixtures need them.
- **Dependencies:** `toml` (workspace `toml = "1"`) as a dev-dependency, to parse scenes. `serde` and `serde_json` are already dependencies. Nothing new in `Cargo.lock`.
- **Tools:** `ffmpeg` with `libx264` and `libvpx-vp9`, needed only by the ignored film test.
- **Repository files:** this change ships two scenes under `crates/querymt-gpui/film/scenes/`:
  - `smoke.toml`, which its own verification uses;
  - `prompt-and-reply.toml`, the spike's clip as data.

  The companion change moves clip scenes into its `demos/` tree.
- **Special builds:** wasm and release builds compile no film code. The composer id compiles everywhere.
- **CI:** unchanged. The regular test suite still has to pass with the composer id.
- **querymt-desktop:** no UI follow-up.
