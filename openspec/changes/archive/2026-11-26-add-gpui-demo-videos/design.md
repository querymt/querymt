# Design

## Context

See proposal.md for motivation. This change consumes the outputs of `add-gpui-film-scenes` (its design D2 scene format, D5 tracks and D6 timing file):

- `target/film/<name>/main.mp4`: opaque, 2x the scene's logical size (2560x1600 for 1280x800), 30 fps.
- `target/film/<name>/panel.webm`: VP9 `yuva420p` with `alpha_mode=1`, a fixed canvas, the same frame count. Only when the scene uses the quick panel in `track` mode.
- `target/film/<name>/<name>.film.json` and `film.js`: `window.QMT_FILM` holding fps, frames, scale, canvas, tracks (panel anchor, bounds and visible ranges), events (`frame`, `t`, `kind`, `window`, `target`, `rect`, `pct`), marks and captions.

**HyperFrames, as documented** (`hyperframes.heygen.com`, CLI 0.9.98 on npm, `engines.node >= 22`):

- A project is a folder with `index.html` (root composition: `data-composition-id`, `data-width`, `data-height`, `data-duration`; optional `data-fps`), `hyperframes.json`, `compositions/` and `assets/`.
- Media sources must sit inside the project folder. Remote or out-of-project sources are rejected.
- Timed elements use `class="clip"` with `data-start`, `data-duration`, `data-track-index`; video trims use `data-media-start`.
- Animation is a paused GSAP timeline registered on `window.__timelines[<composition-id>]`. The rules are no wall clock, no unseeded randomness, no mid-render fetch, and a fixed size and length.
- Variables are declared in `data-composition-variables` on the root and overridden with `render --variables-file <json>`.
- The CLI commands are `lint [dir]`, `check [dir] [--at …]`, `snapshot --at …`, `render [--composition] [--output] [--variables-file] [--quality] [--fps]`, `preview`, and `doctor`.
- Chrome honours WebM alpha only for `yuva420p` with `alpha_mode=1`, which the film harness writes.
- The catalog has `browser-device-stage` (device chrome with a video slot; its "window" chrome is the pattern) and cursor components (`cursor-zoom-follow`, `agent-cursor`). Those take a fixed set of click variables (for example `click1_x … click3_y`), so they cannot be driven directly by a variable-length event list.

**On the development machine:** Node 26, `npx`, and ffmpeg 9.1 with `libvpx-vp9`.

## Goals / Non-Goals

**Goals:**

- `demos/film.sh <clip>` turns a scene into a finished 1920x1080 MP4 with no manual step.
- A clip is two small files (`scene.toml`, `clip.json`); everything else is shared.
- Composition-only changes re-render without re-filming (`--no-film`).

**Non-Goals:**

- Pixel identity across machines (no `--docker`).
- An editor workflow. `npx hyperframes@0.9.98 preview target/demos/<clip>` works for looking, but edits made there are not written back to `demos/`.

## Decisions

### D1. Layout: sources in `demos/`, generated projects in `target/demos/`

```text
demos/
├── README.md
├── film.sh                      # entry point
├── build.mjs                    # assembles one HyperFrames project (Node, no npm deps)
├── shared/
│   ├── app-clip.html            # root composition template
│   └── hyperframes.json         # project settings, copied as is
└── clips/
    ├── prompt-and-reply/
    │   ├── scene.toml           # add-gpui-film-scenes format
    │   └── clip.json            # HyperFrames variable overrides
    └── quick-chat/
        ├── scene.toml
        └── clip.json

target/demos/<clip>/             # generated, ignored by /target
├── index.html                   # from app-clip.html, sized and timed from the timing file
├── hyperframes.json
└── assets/ { main.mp4, panel.webm?, film.js }
target/demos/<clip>.mp4          # the finished video
```

- **Why generate a project per clip:** HyperFrames rejects out-of-project media, and the root's `data-duration` must be known before frame 0. Both come from the film run. Generating keeps `demos/` free of footage and per-run numbers.
- **Rejected: one HyperFrames project with a sub-composition per clip.** It needs footage committed or symlinked into one project, and a root whose length changes with every clip.
- **Rejected: catalog components as-is.** Their fixed click slots do not fit variable-length event lists (Context). `app-clip.html` borrows their look (window chrome, click ring, spring follow), not their code.

### D2. `film.sh`

```text
film.sh [--no-film] [--all | <clip>…]
```

For each clip:

1. Unless `--no-film`:

   ```bash
   QMT_FILM_SCENE=demos/clips/<clip>/scene.toml cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(=film::film_scene)' --no-capture
   ```

   The scene's `name` must equal `<clip>`; `build.mjs` checks this.
2. `node demos/build.mjs <clip>`: reads `target/film/<clip>/<clip>.film.json` and writes `target/demos/<clip>/` (D3).
3. `npx --yes hyperframes@$HYPERFRAMES lint target/demos/<clip>`. Errors stop the clip.
4. `npx --yes hyperframes@$HYPERFRAMES render target/demos/<clip> --variables-file demos/clips/<clip>/clip.json --output target/demos/<clip>.mp4 --quality delivery`.

   Whether `render` takes the project directory as a positional argument or must run with it as the working directory is checked against `render --help` 0.9.98 in task 1.2. The script uses whichever works.

Other behaviour:

- `HYPERFRAMES=0.9.98` is set at the top of the script, overridable from the environment. `FILM_OUT` defaults to `target/film`, and the script exports `QMT_FILM_OUT` so both sides agree.
- Prerequisites are checked up front, each with a one-line fix: Node ≥ 22 (`node -p process.versions.node`), `ffmpeg` with `libvpx-vp9` (only when any selected scene uses the panel), and `cargo nextest`.
- `set -euo pipefail`, run from the repository root (`cd "$(git rev-parse --show-toplevel)"`).
- `--all` runs every directory under `demos/clips/`, in order. A failing clip stops the run with its name.

### D3. `build.mjs`

Plain Node ESM using only `fs` and `path`:

- **Input checks:** the timing file has `version === 1` and `name === <clip>`, and every track file exists.
- **Assets:** copies `main.mp4`, `panel.webm` (if listed) and `film.js` into `assets/`, and `shared/hyperframes.json` alongside. As built, it also cuts `first.png` and `last.png` from `main.mp4` with `ffmpeg`, which hold the footage under a disabled card.
- **`index.html`:** produced from `shared/app-clip.html` by replacing placeholders (the first four below; as built, also `{{FOOTAGE_S}}`, `{{FOOTAGE_AT}}`, `{{END_AT}}`, `{{END_S}}` and `{{PANEL_VIDEO}}`, which keep media timings static):

  | Placeholder | Value |
  |---|---|
  | `{{DURATION}}` | `frames / fps` plus the title and end card lengths from the template's defaults |
  | `{{FPS}}` | `fps` |
  | `{{FOOTAGE_W}}`, `{{FOOTAGE_H}}` | the canvas's logical size |

  Card lengths are fixed in the template, so the root duration does not depend on variables: a disabled card is shown as a hold of the first or last footage frame. Unknown `{{…}}` placeholders fail the build.
- **Output:** prints the project path and duration.
- **Tests:** `node --test demos/build.test.mjs` covers placeholder replacement, the version and name checks, and the missing-track error, using a fixture timing file written to a temporary folder.

### D4. The shared composition `app-clip.html`

- **Root:** `data-composition-id="app-clip"`, 1920x1080, `data-fps="{{FPS}}"`, `data-duration="{{DURATION}}"`.
- **Variables (`data-composition-variables`, on the `<html>` element as an array of declarations; `background` is a string, since the declaration types are string, number, boolean and color):**

  | Variable | Type | Default |
  |---|---|---|
  | `title` | string | — |
  | `subtitle` | string | — |
  | `show_title_card` | boolean | true |
  | `show_end_card` | boolean | true |
  | `cursor` | boolean | true |
  | `zoom` | number, 1.0–2.0 | 1.35 |
  | `background` | enum: `dark`, `light`, `accent` | — |
  | `accent` | color | `#f97316` |
- **Structure, back to front:**
  1. a background (a gradient by `background`);
  2. the window: rounded, fitted to 80% of the stage at the footage's aspect ratio. It has **no title bar of its own**: the app's header is transparent and its window buttons are left to macOS (`traffic_light_position` (14, 15) in `src/ui/mod.rs`), which an offscreen film never draws. The three buttons are drawn over the footage at that spot (12 px wide, 20 px apart, scaled with the footage). This replaces the first design's separate title bar and `window_title` variable, which put a second bar above the app's header;
  3. the main `<video class="clip">` with `src="assets/main.mp4"`, timed after the title card;
  4. the panel layer: `<video>` of `assets/panel.webm`, absolutely placed from `tracks[panel].anchor` and the current `bounds`, with a soft shadow and a backdrop blur over the main footage, shown during `visible` ranges;
  5. the cursor (an SVG arrow; an I-beam during `type` events);
  6. the click ring;
  7. captions (a lower third);
  8. the title and end cards.
- **Timeline:** one paused GSAP timeline, built once at load from `window.QMT_FILM` (a plain `<script src="assets/film.js">` before the composition script) and registered on `window.__timelines["app-clip"]`. All tweens are positioned explicitly in seconds, `fromTo` where both ends matter.
  - **Cursor:** for each event with a `rect`, move to its centre (or the text start for `type`), arriving 0.12 s before `t`. The duration scales with distance and is capped at 0.6 s, eased `power2.inOut`. Positions are `pct` mapped into the window frame's screen rect, so they need no pixel maths at run time.
  - **Click:** a ring scales 0.4→1.4 and fades over 0.35 s at `t`, plus a 0.08 s press of the cursor.
  - **Zoom:** for each `mark`, and for `click` and `type` events, push the window frame to scale `zoom` towards the target over 0.6 s. Hold while events stay within 30% of the frame of each other, then return to 1.0 over 0.6 s once 1.2 s pass with no new events. These rules are written as a pure function `zoomPlan(events, marks)` in the template, so they can be read and tested apart from the DOM.
  - **Captions:** fade and lift in at `from` and fade out at `to`.
  - **Panel layer:** placement changes at the `bounds` keyframes are `set`, not tweens, because the footage already contains the panel's own animation.
- **Determinism:** no `Date.now`, no `Math.random`, no `requestAnimationFrame`, and no fetches beyond GSAP and the local assets. `lint` and `check` confirm this.

### D5. Clips

- **`prompt-and-reply`:** the scene moves from `crates/querymt-gpui/film/scenes/prompt-and-reply.toml`. `clip.json` sets `title` "Ask, and watch it stream" and `subtitle` "QueryMT Desktop".
- **`quick-chat`:** a fixture world with a few sessions and an open session in the main window. It opens the quick panel, types a question, presses Enter, streams a short answer, ends the turn, then hands off to the main window with `submit_and_open` (the scene presses `cmd-enter` in the panel; the panel has no element id for an open button). `panel = "track"`.

## Risks / Trade-offs

- **[`render` or `lint` CLI shapes differ from the docs in 0.9.98]** → Check `--help` first (task 1.2); `film.sh` follows the installed CLI.
- **[The WebM panel shows black corners in Chrome]** (alpha not honoured) → Check with `npx hyperframes@0.9.98 snapshot` on the panel's first visible frame (task 3.2). Fallback: the scene's `panel = "composite"`, which drops the backdrop blur.
- **[Backdrop blur over a `<video>` is slow or ignored in headless Chrome]** → Measure render time. If it is ignored, keep the shadow only and record it.
- **[jsDelivr is unavailable during a render]** → The render fails loudly. Accepted, because HyperFrames' own docs use the CDN. Vendoring GSAP into `demos/shared/` is the follow-up if this bites.
- **[First `npx` run downloads HyperFrames and Chrome]** → Documented in the README; `npx hyperframes@0.9.98 doctor` is the first step there.
- **[Trade-off: pinned via `npx` only]** The version is pinned in `film.sh`, but transitive npm dependencies are not locked. Accepted (user decision); renders on one machine stay consistent while npm's cache holds.

## Open Questions

None that change this plan. CLI argument shapes and alpha playback are checked in the first tasks.
