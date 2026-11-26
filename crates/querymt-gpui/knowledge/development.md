---
id: gpui-development
title: Building, testing and changing querymt-gpui
description: Build targets (native, wasm, Nix), where tests live and how CI runs them, golden files, the in-workspace session core, fonts, release packaging and the OpenSpec change workflow.
tags: build, test, ci, nextest, clippy, wasm, web, nix, golden, fonts, session core, release, packaging, openspec, workflow
knowledge_paths: ["crates/querymt-gpui/Cargo.toml", "crates/querymt-gpui/build.rs", "crates/querymt-gpui/web/build.sh", ".github/workflows/test-gpui.yml", "crates/querymt-gpui/src/film/*.rs", "crates/querymt-gpui/film/scenes/*.toml", "demos/README.md", "demos/film.sh", "demos/build.mjs", "demos/clips/*/scene.toml"]
knowledge_sources: ["crates/querymt-gpui/Cargo.toml", ".github/workflows/test-gpui.yml", "crates/querymt-gpui/web/build.sh"]
---

# Building, testing and changing querymt-gpui

The app lives in the QueryMT workspace as `crates/querymt-gpui` (package `querymt-gpui`).
Paths in these notes and in the GPUI OpenSpec changes (`src/...`, `tests/...`, `web/...`)
are relative to that directory unless they start with `crates/`.

## One binary crate

There is no library target: `src/main.rs` is the crate root for both the native binary and
the wasm32 build. All unit and GPUI tests live in the **bin target** (`#[cfg(test)]` modules,
some in separate files such as `src/quick/tests.rs`), so test commands need `--bins`.
Errors are plain `String`s (no `anyhow`/`thiserror`). Prefer `pub(crate)` for new APIs;
older code still has many plain `pub` items.

The crate is a workspace member but not a default member: plain `cargo build` at the
repository root does not build it (it needs GPUI's system libraries on Linux). Always pass
`-p querymt-gpui`.

## Commands (from the repository root)

```bash
nix develop -c cargo run -p querymt-gpui                              # native; qmtcode on PATH becomes "Local qmtcode"
nix develop -c cargo run -p querymt-gpui -- --agent 127.0.0.1:42069   # add a WebSocket agent (also QMT_AGENT)
QMT_DEBUG=1 nix develop -c cargo run -p querymt-gpui                  # agent/RPC traffic to stderr
cargo r-gpui [--agent host:port]                                      # optimized (release profile) build and run
cargo t-gpui                                                          # tests, as CI runs them (nextest --bins)
cargo cl-gpui                                                         # clippy -D warnings
cargo fmt --all -- --check
nix develop -c crates/querymt-gpui/web/build.sh [--release]           # wasm into crates/querymt-gpui/dist
```

CI (`.github/workflows/test-gpui.yml`): clippy with `-D warnings`, then
`cargo llvm-cov nextest -p querymt-gpui --bins --locked` on Ubuntu, and the release web
build. It runs when the crate, `crates/agent-session-core`, `crates/agent-protocol-types`
or the workspace manifests change. Formatting and the workspace-hack check are in
`lint.yml`. Code must compile on both native and `wasm32-unknown-unknown`: native-only
code (quick panel, processes, sockets, tray) is gated with
`#[cfg(not(target_family = "wasm"))]`.

## Web build

`web/build.sh` builds with `RUSTC_BOOTSTRAP=1` (the `wasm_thread` dependency uses a nightly
feature gate; the app still runs single-threaded), then `wasm-bindgen` and, for release
(`--profile release-gpui-web`), `wasm-opt`. The `wasm-bindgen` CLI in the root `flake.nix`
and in the CI workflows must match the `wasm-bindgen` version pinned in the workspace
`Cargo.lock`. Icons used on the web must be listed in `icon_assets!` in `src/main.rs`.

## Fonts

`build.rs` downloads five IBM Plex fonts from a pinned commit, verifies SHA-256 and caches
them in the OS cache dir (`QMT_FONT_CACHE` overrides). `CARGO_NET_OFFLINE=true` refuses to
download; the first uncached build needs network. The Nix build pre-fetches them.

## Golden session files

`src/session/golden.rs` compares projected transcript rows with `tests/golden/session/`.
A deliberate change of what rows show: rerun with `UPDATE_GOLDEN=1` and review the diff.
The same test replays the session core's seeded golden cases
(`querymt_agent_session_core::testing::golden_cases()`), so a change to the core that alters
rows shows up here in the same pull request.

## Session core

`querymt-agent-session-core` is a path dependency (`crates/agent-session-core`). There is no
pin to bump: a change to the core or to `querymt-agent-protocol-types` must keep
`cargo t-gpui` green.

## Filming demo clips

`src/film/` (test-only, OpenSpec changes `spike-gpui-demo-film` and `add-gpui-film-scenes`)
renders the real `QmtApp` offscreen, frame by frame, on GPUI's `HeadlessAppContext` with a
stepped fake clock, and pipes the frames to `ffmpeg`. A clip is a **scene file** (TOML) read
at run time, so editing one needs no recompile. The film tests are `#[ignore]`d, so
`cargo t-gpui` and CI never run them; only the scene, agent, timing and mask unit tests and
the composer-id test run there. That test opens a real (headless) window, so on Linux it needs a
Vulkan or GL driver; on a machine without a GPU, install Mesa's software Vulkan driver
(`mesa-vulkan-drivers`, lavapipe), as the CI `test` job does.

```bash
QMT_FILM_SCENE=crates/querymt-gpui/film/scenes/smoke.toml \
  cargo nextest run -p querymt-gpui --bins --run-ignored only \
  -E 'test(=film::film_scene)' --no-capture
```

- Needs `ffmpeg` with `libx264` and `libvpx-vp9` (and `ffprobe`, to inspect) on `PATH`.
- Environment: `QMT_FILM_SCENE` (a path relative to the repository root, or absolute; the test
  fails with a message when unset), `QMT_FILM_OUT` (output base, default `target/film/`),
  `QMT_FILM_AGENT` (`host:port` of a seeded world; overrides the scene's `address`),
  `QMT_FILM_PANEL_CODEC=prores` (panel track as ProRes 4444 `panel.mov` instead of VP9 alpha).
- Harness scenes live in `crates/querymt-gpui/film/scenes/`: `smoke.toml` (the harness's own
  check, also `film::smoke_scene`) and `smoke-panel.toml` (the quick panel). Scenes for demo
  videos live with their clip, in `demos/clips/<clip>/scene.toml`, for example
  `demos/clips/prompt-and-reply/scene.toml` (the spike's clip).

### Demo videos

`demos/` turns filmed scenes into finished 1920x1080 videos (window frame, cursor, zoom, captions,
title cards) with HyperFrames: `demos/film.sh <clip>` films, assembles and renders. See
`demos/README.md` for the prerequisites, how to add a clip and the composition variables.

### Scene format (`version = 1`)

```toml
version = 1
name = "prompt-and-reply"          # output folder and file stem
fps = 30
size = [1280, 800]                 # logical; captured at the test window's 2x
theme = "dark"                     # "dark" | "light"
accent = "orange"
panel = "track"                    # "track" (separate alpha track) | "composite"
anchor = { x = 0.5, bottom = 0.15 } # where the panel sits in the main canvas
stills = [0, 90]                   # frames also written as still-<frame>.png

[world]
kind = "fixture"                   # "fixture" | "agent" (seeded from a running qmtcode)
agent_title = "qmtcode"
cwd = "/workspace/demo"
model = "anthropic:claude-sonnet-4-5"
models = [{ id = "anthropic:claude-sonnet-4-5", label = "Claude Sonnet 4.5", provider = "anthropic" }]
sessions = [{ id = "s-1", title = "Refactor the parser", cwd = "/workspace/demo", updated = "2026-11-20T10:00:00Z", phase = "idle" }]
session = { id = "film", title = "Streaming replies" }   # open in the main pane; omit for the Start page

[[step]]
at = 0.3                           # seconds from the start; or `after = 0.5` = after the previous step ends
type = "How do I stream a reply into a GPUI view?"
cps = 18                           # characters per second (default 18)
caption = "Ask anything"           # `caption` and `window = "main" | "panel"` go on any step

[[step]]
after = 0.3
press = "enter"

[[step]]
after = 0.4
stream = """## Streaming a reply
..."""
wps = 9                            # words per second (default 9)

[[step]]
after = 0.2
end_turn = "end_turn"              # stop reason; answers the held session/prompt

[[step]]
at = 6.0
end = true                         # the last frame; required, and last
```

- One action key per step: `wait` (seconds), `type`, `press`, `click`, `hover`, `scroll`
  (`{ target, dx, dy }`), `quick_open`, `quick_close`, `stream`, `tool_call` (`{ id, title,
  kind, status, content }`), `state` (`"running"` or `"idle"`), `end_turn`, `notify` (`{ method,
  params }`, raw JSON), `mark` (`{ name, target }`) and `end`.
- Targets (`click`, `hover`, `scroll`, `mark`): an element id (`"composer-input"`,
  `"find-close"`), resolved with `TestWindowExt::find` in the step's window, or `{ at = [x, y] }`
  in logical window pixels, sent as raw mouse events. Debug selectors cannot be targeted: on a
  `HeadlessAppContext` window their bounds are private to GPUI.
- Validation runs before frame 0, and errors name the step index and key: times may not go
  back, one action per step, `end` last, a seeded world takes no agent steps, `window =
  "panel"` needs an earlier `quick_open` and no `quick_close` since. A step overlapping a
  running `type` or `stream` is allowed.
- Durations: `type` lasts chars / `cps`, `stream` words / `wps`; times round to whole frames and
  the clip lasts up to the `end` step's frame. Typing and streaming deliver one chunk per frame
  (a reply is cut at whitespace only).

### The fake agent

A fixture world has no agent: the harness answers the app's requests every frame, as the
quick-panel tests do. `session/new` gets the world session's id (else `q1`); `session/prompt`
is held open until `end_turn`, and is acknowledged by echoing the prompt as
`user_message_chunk` (with the request's `client_prompt_id`), as a real agent does, which
turns the app's "Sending prompt…" row into the user's message; everything else gets `{}`.
`stream`, `tool_call`, `state` and `notify` target the open turn's session, else the world's.
A prompt typed into the composer and sent with Enter therefore goes through the app's real send
path. A seeded world (`kind = "agent"`) talks to the real agent instead and refuses agent steps.

### Outputs

`$QMT_FILM_OUT/<name>/` holds `main.mp4` (H.264, 30 fps, 2x capture), `<name>.film.json` and
`film.js` (the same object as `window.QMT_FILM = {...};`), `still-<frame>.png` for `stills`, and
`panel.webm` (or `panel.mov`) when the scene opens the panel in `track` mode. The timing file
(`version = 1`) has `fps`, `frames`, `scale`, `canvas`, `tracks` (one per window: `file`,
`size`, `anchor`, the `visible` frame ranges and `bounds` as `[frame, x, y, w, h]` where the
size changed), `events` (every input and agent step: `frame`, `t`, `kind`, `window`, `target`,
`rect` in logical pixels and `pct` in % of the window's canvas, `text`, `key`, `until`), `marks`
and `captions` (each lasts until the next one). For the panel, `rect` and `bounds` are in its
track's canvas (or the main canvas in `composite` mode), not raw window coordinates.

### The quick panel

The film world installs `QuickHost` when a step uses the panel (`prefs.quick.enabled` stays
off, so no hotkey or tray starts) and opens it with `host.show`. The panel films with its opaque
"Reduce transparency" chrome (`Chrome::solid()`); there is no native material (Liquid Glass or
visual effect), edge light, window shadow, global hotkey, tray or dragging.

- `panel = "track"`: its own track on a fixed canvas of `quick::panel::WIDTH` x `MAX_HEIGHT`
  (680 x 760 logical), bottom-centred, encoded as VP9 with alpha (`yuva420p`, `alpha_mode=1`).
  Capture is opaque (the offscreen Metal renderer clears to alpha 1), so the harness multiplies
  the alpha by a rounded-rectangle mask at `chrome::RADIUS`; the track is fully transparent
  while the panel is closed, and has as many frames as `main.mp4`. ffprobe's native VP9 decoder
  ignores the alpha plane: decode with `-c:v libvpx-vp9` to see it.
- `panel = "composite"`: the masked panel and a soft shadow are blended onto the main frame at
  the scene's `anchor`, into `main.mp4` alone.
- The test window changes its size on `resize` without telling GPUI, so `Film::draw` calls
  `Window::bounds_changed` when the viewport differs from the bounds (the panel grows with its
  chat; without it only the bar is laid out).
- Inside the panel, `type`/`press` steps carry no target while it is only a bar: the bar's
  textarea has no element id that `find` resolves. Use a point target.

### Limits

- The composer's input has the element id `composer-input` (`.id(..).test_support()` in
  `ui/session_view.rs`; a plain id is not found by `find`, and `test_support` is inert without
  the kit's `test-support` feature). The id is unscoped, so a scene with split panes cannot
  tell composers apart.
- The window is 1280x800 logical at the test platform's 2x scale, with no OS frame or traffic
  lights. The theme and accent come from the scene (`prefs.dark` and `prefs.accent` in memory,
  because the test window's appearance is light). Capture takes 125-145 ms per frame (Apple
  M-series, Metal, unoptimized build), so a clip renders slower than real time. On macOS the
  text system is `gpui_apple::AppleTextSystem`, a macOS-only dev-dependency, because
  `current_platform` builds a `MacPlatform`, which panics off the process main thread. Find in
  session does nothing on an empty transcript.
- A seeded world (`kind = "agent"`: `launch::websocket_fixture` and the real
  `AgentsStore::connect`, waiting up to 20 s for sessions and models) is not reproducible: the
  frame shows the agent's real sessions, and the store re-lists them after scripted updates.
  Point it at a qmtcode with a demo workspace.
- Linux (wgpu) and Windows filming are untried; the composer-id test runs everywhere but is
  unverified on Linux CI.

## Releases

The app ships with the main QueryMT release and nightly (`.github/workflows/build.yml`) as
`QueryMT-Desktop-<version>-<platform>` bundles plus `querymt-desktop-web-<version>.tar.gz`.
The version is the workspace version; CI passes the exact release or nightly version as
`QMT_VERSION`, which the app reports instead of `CARGO_PKG_VERSION` (`util::app_version`).
Native release builds use the `release-gpui` profile (size-optimized, fat LTO, abort on
panic) and are packaged by `scripts/package/{macos,windows,linux}.sh`.

The web build is also the `qmtcode --dashboard` UI: the `dashboard` feature of
`querymt-agent` embeds it. `crates/agent/build.rs` (`build_support/gpui_web.rs`) builds
this crate for wasm32 with `release-gpui-web` in `target/querymt-gpui-web`, unless
`QMT_UI_DIST` names a prebuilt directory (CI passes the `desktop-web` job's output; Nix
passes `packages.querymt-gpui-web`). A web-breaking change here therefore breaks
`cargo build -p querymt-agent --features dashboard` without `QMT_UI_DIST`; the
`dashboard-embed` job in `test-gpui.yml` builds that path, serves the page from
qmtcode and checks in headless Chrome that the app starts. The agent's own checks
(`cargo t-agent`, `cargo cl-agent`, `test-agent.yml`) use `dashboard-server`, which
serves a placeholder page and never builds this crate.

The embedded files are stored gzipped (`compress_assets` in `gpui_web.rs`: text and wasm
over 1 KiB become `<name>.gz`), and `crates/agent/src/server.rs` sends them with
`Content-Encoding: gzip`, `ETag` and `Cache-Control: no-cache` (304 on a matching
`If-None-Match`). The `Content-Type` comes from the requested name, so the wasm stays
`application/wasm`.

## Changing behaviour: OpenSpec

Non-trivial changes go through OpenSpec in the repository root (`openspec/`, skills
`openspec-propose`, `openspec-apply-change`, `openspec-archive-change`):

- `openspec/specs/<capability>/spec.md`: accepted behaviour.
- `openspec/changes/<change>/`: `proposal.md`, `design.md` (numbered decisions D1, D2, …
  that module docs cite; every change starts at D1, so name the change when citing one
  that is not the file's usual one, see [README.md](README.md)), `tasks.md`, and delta
  specs.
- Archiving moves a change to `openspec/changes/archive/<date>-<change>/` and merges its
  delta specs into `openspec/specs/`.

## Repository knowledge

These notes are indexed by QueryMT's repository knowledge (the root `.qmt/knowledge.toml`).
After moving files or rewriting a note, run the check:

```bash
cargo run -p querymt-agent --example repository_knowledge -- --root . check
```
