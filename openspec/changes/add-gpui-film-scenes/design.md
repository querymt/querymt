# Design

## Context

See proposal.md for motivation. Starting point: the archived spike `2026-11-26-spike-gpui-demo-film` (findings in its design.md "Spike findings" and tasks.md), implemented in `crates/querymt-gpui/src/film.rs` (commit `fe53493b`).

**Already in place:**

- **Film world:** `HeadlessAppContext::with_platform`, with `gpui_apple::AppleTextSystem` on macOS (macOS-only dev-dependency `gpui-pre-apple = "=0.3.14"`) and `gpui_kit::platform::current_headless_renderer`. The bootstrap repeats `main.rs` `run`: `gpui_kit::init`, `install_fonts`, `theme::apply`, `ui::bind_keys`.
- **Starting state** (`World`):
  - `Fixture`: `launch::peer_fixture()`;
  - `Agent(address)`: `launch::websocket_fixture`, then `AgentsStore::connect`, then a wall-clock wait for the session list and model catalog.
- **Main window:** `install_main_for_test` plus `QmtApp::show_session_for_test`. The theme is forced with `prefs.dark = Some(true)` and `apply_theme`.
- **Frame loop:** apply due steps with `notify_for_test`, `advance_clock(1/fps)` (cumulative nanoseconds), `run_until_parked`, `render_frame`, `capture_screenshot`, then raw RGBA to `ffmpeg` over stdin.
- **Input on a headless window** (gpui-kit `TestWindowExt`):
  - `press(key)`, `input(text)` (to the focused element), `click`, `hover`, `scroll` and `drag_to` take an element id;
  - `find(id)` and `try_find(id)` return an `ElementSnapshot` with `bounds()`.

  Debug-selector bounds are only reachable through `VisualTestContext::debug_bounds`. On a `HeadlessAppContext` window they are `pub(crate)` in GPUI (`Window::rendered_frame`), so this harness cannot target elements by debug selector. `Window::dispatch_event(PlatformInput, cx)` is public, so raw mouse events at a point are possible.
- **Element registration:** `TestWindowExt::find` sees only elements registered through gpui-base's observation (`gpui_base::test_support`). gpui-kit components such as `kit::icon_button("find-close", …)` register; whether a bare `div().id(..)` does is unverified.
- **Composer:** `src/ui/session_view.rs` builds it as `self.slash_keys(div().relative(), cx).debug_selector(|| "composer-input".into())`. The same composer renders in the main window and, compact, in the quick panel.
- **Quick panel:**
  - test rig: `QuickHost::install(store, Some((main, app)), cx)`, then `host.show(cx)`, then `host.panel_for_test()` → `(AnyWindowHandle, Entity<QuickPanel>)`;
  - `QuickPanel::submit` / `submit_and_open`;
  - the fake agent's replies are modelled by `Rig::pump`: `session/new` → `{ "sessionId": "q1" }`, `session/prompt` held, everything else `{}`;
  - `chrome::install` returns `Chrome::solid()` off the main thread (`quick/chrome/macos.rs`, no `MainThreadMarker`);
  - the panel window is `WindowBackgroundAppearance::Transparent`, its root has radius `chrome::RADIUS` (20), and it resizes as the chat grows (`chrome::resize` → `window.resize` on the test window).
- **Capture is opaque:** `MetalHeadlessRenderer::new` → `MetalRenderer::new_headless` passes `opaque = true`, which clears to alpha 1. Captured panels have black corners, not transparent ones.
- **Request channel:** `AgentsStore::rpc_for_test(agent)` returns `(Rc<RpcClient>, Receiver<String>)`. A reply goes back through `rpc.handle(json)`.
- **Sending a prompt:** the view shows the user's message immediately (`ItemKind::User { local: true }`, "Sending prompt…"), so a prompt typed and sent through the composer appears without a scripted `user_message_chunk`.
- **Fixture data helpers:** `runtime_mut_for_test(agent).sessions` with `sessions_loaded`, and `launch::add_models_for_test`.
- **Toolchain:** `toml = "1"` is a workspace dependency. ffmpeg 9.1 on the dev machine has `libx264`, `libvpx-vp9` and `prores_ks`.

## Goals / Non-Goals

**Goals:**

- A clip is a scene file. Editing it and rerunning films the new clip without recompiling.
- Same scene and same machine give the same frames. Fixture worlds use no wall clock and no network.
- Everything a video tool needs to place a cursor, zooms, captions and the panel is in the timing file.
- The quick panel can be filmed as its own transparent track.

**Non-Goals:**

- A stable public format. The scene and timing formats are versioned (`version = 1`) but internal to this repository.
- Pixel parity with the native panel material.

## Decisions

### D1. Module layout and entry point

`src/film.rs` becomes `src/film/` (`#[cfg(test)] mod film;` is unchanged):

| File | Holds |
|---|---|
| `mod.rs` | the world bootstrap (today's `open`), `Film`, the frame loop, and the tests |
| `scene.rs` | serde types for the scene file, parsing and validation (unit-tested) |
| `agent.rs` | the fake agent: request auto-replies and notification builders (unit-tested) |
| `track.rs` | per-window capture, canvas placement, alpha mask, encoder processes (mask math unit-tested) |
| `timing.rs` | timing-file types and `film.json` / `film.js` writers (unit-tested) |

Tests:

- **`film_scene`** (`#[ignore]`): films the scene at `$QMT_FILM_SCENE`, a path relative to the repository root or absolute. It fails with a clear message if the variable is unset.
- **`smoke_scene`** (`#[ignore]`): films `crates/querymt-gpui/film/scenes/smoke.toml` and asserts on the timing file. It is the change's own end-to-end check.
- **Existing tests:** `film_prompt_and_streamed_reply` and `_seeded` are removed. Their clip becomes `film/scenes/prompt-and-reply.toml` (moved to `demos/` by the companion change). `probe_headless_input` and `probe_seeded_from_agent` are removed; their findings are archived.
- **Run command:**

  ```bash
  QMT_FILM_SCENE=<path> cargo nextest run -p querymt-gpui --bins --run-ignored only \
    -E 'test(=film::film_scene)' --no-capture
  ```

  The scene file is read at run time and is not an `include_str!`, so cargo does not rebuild when it changes.

**Rejected: an `example` or a second binary.** The crate has no library target, and the fixtures are `#[cfg(test)]` (spike D1, still true).

### D2. Scene file (TOML, `version = 1`)

```toml
version = 1
name = "prompt-and-reply"          # output folder and file stem
fps = 30
size = [1280, 800]                 # logical; captured at the test window's 2x
theme = "dark"                     # "dark" | "light"
accent = "orange"
panel = "track"                    # "track" (separate alpha track) | "composite"
stills = [0, 90]                   # frames also written as PNG

[world]
kind = "fixture"                   # "fixture" | "agent"
# address = "localhost:3030"       # kind = "agent" only; `$QMT_FILM_AGENT` overrides
agent_title = "qmtcode"
cwd = "/workspace/demo"
model = "anthropic:claude-sonnet-4-5"
models = [{ id = "anthropic:claude-sonnet-4-5", label = "Claude Sonnet 4.5", provider = "anthropic" }]
sessions = [{ id = "s-1", title = "Refactor the parser", cwd = "/workspace/demo", updated = "2026-11-20T10:00:00Z", phase = "idle" }]
session = { id = "film", title = "Streaming replies" }   # the session open in the main pane; omit for the Start page

[[step]]
at = 0.3                           # seconds from the start; or `after = 0.5` = after the previous step ends
type = "How do I stream a reply into a GPUI view?"
cps = 18                           # characters per second
caption = "Ask anything"

[[step]]
after = 0.3
press = "enter"

[[step]]
after = 0.4
stream = """## Streaming a reply
..."""
wps = 9                            # words per second

[[step]]
after = 0.2
end_turn = "end_turn"              # stop reason; answers the held session/prompt

[[step]]
at = 6.0
end = true                         # last frame; required
```

- **One action key per step:**
  - `wait`, `type`, `press`, `click`, `hover`, `scroll`;
  - `quick_open` (bool) and `quick_close`;
  - `stream`, `tool_call` (an inline table: `id`, `title`, `kind`, `status`, optional `content`), `state` (`"running"` or `"idle"`), `end_turn`, `notify` (`{ method, params }` as raw JSON);
  - `mark` (`{ name, target }`);
  - `end`.

  `caption` (string) and `window` (`"main"` default, or `"panel"`) are allowed on any step.
- **Targets** (`click`, `hover`, `scroll`, `mark`): an element id string, `"composer-input"` or `"find-close"`, resolved with `find` in the step's window. Or an inline table `{ at = [x, y] }` in logical window coordinates, dispatched as raw mouse events with `Window::dispatch_event`. Debug selectors are not supported (see Context).
- **Validation before frame 0** (parse errors name the step index and key):
  - times must be monotonic;
  - each step has exactly one action;
  - `end` exists and is last;
  - `world.kind = "agent"` only with an `address` or `$QMT_FILM_AGENT`;
  - `panel` steps need an earlier `quick_open`.
- **Durations:**
  - `type` lasts `len / cps`; `stream` lasts `words / wps`;
  - `after` counts from the end of the previous step;
  - everything else is instantaneous;
  - the scene length is the `end` step's time;
  - all times are rounded to whole frames.

**Why TOML:** it is readable for prose-heavy `stream` text, it is a workspace dependency already, and comments are allowed. **Rejected: JSON**, which is noisy for multi-line Markdown and has no comments. **Rejected: a Rust builder DSL**, which needs a recompile per edit.

### D3. Fake agent (fixture worlds)

- **Connection:** `rpc_for_test(AGENT)` connects the fixture agent.
- **Every frame**, after `run_until_parked`, the runner drains the request channel and replies:

  | Request | Reply |
  |---|---|
  | `session/new` | `{ "sessionId": <world.session.id or "q1"> }` |
  | `session/prompt` | not answered immediately; remembered as the open turn |
  | `session/load`, `session/list` | `{}` (sessions come from the world, not from the agent) |
  | anything else | `{}` |

- **Turn steps:**
  - `end_turn` replies to the open turn with `{ "stopReason": <value> }` and sends `_querymt/session/stateUpdate` `idle`;
  - `state = "running"` sends the state update the spike used;
  - `stream` and `tool_call` send `session/update` notifications for the open turn's session, with a fixed `messageId` per turn and `runId` in `_meta.querymt` as in the spike.

  Without an open turn they target the world's session, so a scene can script an agent-initiated reply.
- **Seeded worlds (`kind = "agent"`):** the real connection answers requests, and agent steps are refused at validation. They could hit a real model, which the spike deliberately avoided.

### D4. The film loop with several windows

`Film` holds `windows: Vec<FilmWindow { name, handle, track }>`, with `main` first. Each frame:

1. Apply steps due at this frame. Input steps run inside `update_window` on the step's window. Typing runs one `input(chunk)` per frame, with the chunk sized from `cps`. Each step appends an event to the timing log.
2. Advance the clock by one frame, run until parked, and answer requests (D3).
3. For each open window: `render_frame`, `capture_screenshot`, then hand the image to that window's track (D5).

`quick_open`:

- needs `QuickHost::install(store, Some((main, app)), cx)` at world setup when the scene uses the panel;
- calls `host.show`, adds the window from `panel_for_test()` as `panel`, and focuses its composer.

`quick_close` calls `host.hide` and ends the panel track. Later frames of that track are fully transparent, so all tracks have the same frame count and timing.

### D5. Tracks, canvas, alpha mask and encoding

- **Main track:** opaque RGBA piped to `ffmpeg ... -c:v libx264 -pix_fmt yuv420p -crf 18 main.mp4`, as in the spike.
- **Panel track (`panel = "track"`):**
  - the canvas is a fixed size: the panel's width × its maximum height (`quick` module constants; the max height comes from `QuickPanel::set_display_height_for_test` or the panel's cap, decided while implementing and recorded), at 2x;
  - each frame, the panel image is placed bottom-centre on a transparent canvas;
  - the harness multiplies alpha by a rounded-rectangle mask with radius `chrome::RADIUS` × scale, with a one-device-pixel antialiased edge, because capture is opaque;
  - the window's bounds (logical, relative to the main window) are stored per frame in the timing file;
  - encoded with `ffmpeg -f rawvideo -pix_fmt rgba ... -c:v libvpx-vp9 -pix_fmt yuva420p -b:v 0 -crf 24 -auto-alt-ref 0 -metadata:s:v:0 alpha_mode=1 panel.webm`;
  - `$QMT_FILM_PANEL_CODEC=prores` switches to `-c:v prores_ks -profile:v 4444 -pix_fmt yuva444p10le panel.mov`, for editors.
- **Composite (`panel = "composite"`):**
  - no panel encoder;
  - the masked panel is alpha-blended onto the main frame at its bounds, plus a soft drop shadow, as CPU work in `track.rs`;
  - the result is a single `main.mp4`.
- **Panel position in the main frame:** the panel opens at `Bounds::centered(display…)` on the test display, not relative to the main window. The timing file records the main window's and the panel's bounds as the test platform reports them. The composite and the HyperFrames layer place the panel at a scene-chosen anchor (default: centred horizontally, 15% from the bottom of the main canvas), not at real screen coordinates.

### D6. Timing file (`<name>.film.json`, `version = 1`)

```json
{
  "version": 1, "name": "prompt-and-reply", "fps": 30, "frames": 180,
  "scale": 2, "canvas": { "width": 1280, "height": 800 },
  "tracks": [
    { "window": "main",  "file": "main.mp4",  "size": [1280, 800] },
    { "window": "panel", "file": "panel.webm", "size": [680, 560],
      "anchor": { "x": 0.5, "bottom": 0.15 },
      "visible": [[48, 170]],
      "bounds": [[48, 0, 0, 680, 132], [61, 0, 0, 680, 220]] }
  ],
  "events": [
    { "frame": 9, "t": 0.3, "kind": "type", "window": "main", "target": "composer-input",
      "rect": [312, 690, 656, 64], "pct": [24.4, 86.3, 51.3, 8.0], "text": "How do I…", "until": 0.81 },
    { "frame": 33, "t": 1.1, "kind": "press", "window": "main", "key": "enter" }
  ],
  "marks":    [{ "name": "reply", "frame": 120, "rect": [..], "pct": [..] }],
  "captions": [{ "text": "Ask anything", "from": 0.3, "to": 1.1 }]
}
```

- `rect` is in logical window pixels; `pct` is in % of that window's canvas. `bounds` lists only the frames where the panel's size changed (`[frame, x, y, w, h]`).
- A caption lasts until the next caption or the scene end.
- Events with a target always carry the target's bounds at the time of the event, taken from `find(id).bounds()` in the frame that event runs in, or from `at` for point targets.
- `film.js` holds the same object, as `window.QMT_FILM = {…};`, so an HTML composition can include it with a plain `<script src>`.

### D7. The composer element id (product code)

`src/ui/session_view.rs` gets `.id("composer-input")` on the composer input element, next to the existing `debug_selector`. The id is plain, because each window shows one focused composer. A scene that opens split panes must scope targets (`within`); that is not supported in `version = 1` and is noted in the docs.

If `TestWindowExt::try_find("composer-input")` still returns `None` after the id is added (registration, see Context), use the gpui-kit registration that `kit::icon_button` gets. The exact API is taken from the gpui-base source, not invented, and recorded in tasks.md. Point targets (`{ at = [x, y] }`) remain the fallback.

## Risks / Trade-offs

- **[A plain element id is not registered for `find`]** → D7 fallback; point targets always work.
- **[The quick panel needs state the film world lacks]** (prefs `quick.enabled`, hotkey, tray). `QuickHost::sync` starts the hotkey and tray only when `prefs.quick.enabled` is set → keep it disabled in the film world and open with `host.show` directly, as the quick-panel tests do (they never enable it).
- **[The panel window position is unrelated to the main window on the test display]** → D5: the anchor comes from the scene, not from screen coordinates.
- **[VP9 alpha encoding is slow at 2x]** → The panel canvas is smaller than the main canvas. Record the time; ProRes is the fast alternative.
- **[Typing one chunk per frame looks mechanical]** → Accepted for `version = 1`. Chunk sizes follow `cps` exactly. Add jitter later if clips look robotic.
- **[Seeded worlds are not reproducible]** (spike finding) → Fixture is the default. Agent steps are refused for seeded worlds.
- **[Trade-off: a version 1 format with no migration]** Scenes live in this repository and are rewritten when the format changes.

## Open Questions

None that change this plan. Measurements (panel encode time, maximum panel height) are recorded in tasks.md.
