# Tasks

This change sets `skip_specs: true` (developer tooling and media templates), so no task covers spec scenarios. It needs `add-gpui-film-scenes` applied first: the film test `film::film_scene`, the timing file and `panel.webm`. Commands run from the repository root. Findings are recorded under their task, with the date, the machine and the exact command.

## 1. Prerequisites and CLI shapes

- [x] 1.1 Confirm `add-gpui-film-scenes` is applied:

  ```bash
  QMT_FILM_SCENE=crates/querymt-gpui/film/scenes/prompt-and-reply.toml cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(=film::film_scene)' --no-capture
  ```

  This should write `target/film/prompt-and-reply/{main.mp4,prompt-and-reply.film.json,film.js}`.
  - Verify: the command passes and the three files exist.
  - Recorded 2026-11-26, Apple M4 Pro, macOS, nextest 0.9.136: 1 passed, 628 skipped (23.1 s, 150 frames, 2560x1600). `target/film/prompt-and-reply/` held `main.mp4`, `prompt-and-reply.film.json`, `film.js` and two stills. The `smoke-panel` scene also filmed (44 s) and wrote `panel.webm`.
- [x] 1.2 Run `npx --yes hyperframes@0.9.98 doctor`, and record from `--help` for `render`, `lint`, `check` and `snapshot`:
  - whether a project directory is passed positionally or through the working directory;
  - the exact `--variables-file`, `--output` and `--quality` spellings.
  - Verify: the doctor result and the four argument shapes are recorded below this task. D2 of the design follows them.
  - Recorded 2026-11-26, same machine (`npx --yes hyperframes@0.9.98 doctor`): all checks pass (Node 26.13.1, system Chrome, ffmpeg and ffprobe 9.1, Docker). Only the optional Kokoro TTS and MusicGen BGM are missing, so doctor's summary line says "Some checks failed" while the exit code is 0.
  - `render [OPTIONS] [DIR]`, `lint [OPTIONS] [DIR]`, `check [OPTIONS] [DIR]` and `snapshot [OPTIONS] [DIR]` all take the project directory **positionally**, so no `cd` is needed.
  - Flag spellings: `render --variables-file=<path>`, `--output=<path>` (`-o`), `--quality=<draft|looks|delivery|standard|high>` (`-q`); `snapshot --at=<s,s,...>`, `--output=<dir>`, `--no-end`; `check --at=<s,s,...>`, `--no-cache`.
  - Not in the docs the design cites: `snapshot` and `check` have **no** `--variables-file` or `--variables`, so they always show the declared defaults. Frames with a clip's variables applied come from the rendered MP4 (`ffmpeg -ss <t> -i ... -frames:v 1`).
  - `data-composition-variables` goes on the `<html>` element as a JSON array of `{id,type,label,default}`, not on the root `div` (HyperFrames' compositions doc). Variables are read with `window.__hyperframes.getVariables()`.

## 2. Project assembly

- [x] 2.1 Add `demos/shared/hyperframes.json` (settings as `npx hyperframes@0.9.98 init` writes them for a blank project; copy that file's shape, not invented keys) and `demos/shared/app-clip.html`, a first version containing:
  - the root with the `{{DURATION}}`, `{{FPS}}`, `{{FOOTAGE_W}}` and `{{FOOTAGE_H}}` placeholders;
  - the variables from design D4;
  - the background, the window frame, and the main footage clip;
  - title and end cards;
  - a paused GSAP timeline registered as `window.__timelines["app-clip"]`.

  No cursor, zoom, panel or captions yet.
  - Verify: done together with 2.2.
  - Written in one go, with the cursor, zoom, captions and panel layers of group 3 included. Each layer was then verified on its own in group 3.
  - `hyperframes.json` is exactly what `hyperframes init --example blank` writes (`$schema`, `registry`, `paths`, `media.autoProxy`).
- [x] 2.2 Add `demos/build.mjs` (design D3) and `demos/build.test.mjs`.
  - Verify:
    - `node --test demos/build.test.mjs` passes;
    - `node demos/build.mjs prompt-and-reply` writes `target/demos/prompt-and-reply/` and prints its duration;
    - `npx --yes hyperframes@0.9.98 lint target/demos/prompt-and-reply` reports no errors.
  - Recorded: `node --test demos/build.test.mjs` 19 passed, 0 failed (placeholders, version and name checks, missing track, panel track and no-panel project, `zoomPlan`, `zoomTriggers`, `mainPoint`). `build.mjs prompt-and-reply` printed `duration: 10`. `lint`: 0 errors, 4 warnings (file size, track count, nested card elements). They advise splitting into sub-compositions, which the design chose not to do.
  - Deviations from design D3, both needed to keep the root length and media timings static: the template has five more placeholders than the four in the table: `{{FOOTAGE_S}}`, `{{FOOTAGE_AT}}`, `{{END_AT}}` and `{{END_S}}`, plus `{{PANEL_VIDEO}}` (the panel's `<video>`, or nothing; an empty `<video>` is a lint error). Card lengths (2.5 s each) are constants in `build.mjs`. A disabled card is held by `assets/first.png` and `assets/last.png`, which `build.mjs` cuts from `main.mp4` with `ffmpeg` (so "fs and path only" became "plus ffmpeg, already a prerequisite"). A video element cannot hold a frame, so the stills are what the design's "hold of the first or last footage frame" needs.
- [x] 2.3 Add `demos/film.sh` (design D2): prerequisite checks, `--no-film`, `--all`, `HYPERFRAMES` and `QMT_FILM_OUT`; mark it executable.
  - Verify:
    - `bash -n demos/film.sh` passes, and `shellcheck demos/film.sh` passes if installed;
    - `demos/film.sh` with no arguments prints usage and exits 2;
    - with `PATH` lacking `node`, it exits 1 naming Node 22.
  - Recorded: `bash -n` and `shellcheck demos/film.sh` pass (shellcheck installed). No arguments prints usage and exits 2. With `PATH=/tmp/gitonly:/usr/bin:/bin` (git only) it exits 1 with "Node 22 or newer is required ... node is not on PATH".
  - The VP9 prerequisite is checked per clip: a scene with `quick_open` and no `panel = "composite"` needs `libvpx-vp9` (`panel` defaults to `track`).
- [x] 2.4 Move `crates/querymt-gpui/film/scenes/prompt-and-reply.toml` to `demos/clips/prompt-and-reply/scene.toml`, and add its `clip.json` (design D5). Update the film knowledge note's example path.
  - Verify:
    - `demos/film.sh prompt-and-reply` writes `target/demos/prompt-and-reply.mp4`;
    - `ffprobe -v error -show_entries stream=width,height,r_frame_rate:format=duration -of csv=p=0 target/demos/prompt-and-reply.mp4` reports `1920,1080,30/1` and a duration equal to the printed one;
    - a person watches it once (title card, framed footage, end card) and the result is recorded.
  - Recorded: `demos/film.sh prompt-and-reply` wrote `target/demos/prompt-and-reply.mp4`: `1920,1080,30/1` and `10.000000` s, the printed `duration: 10`. Render 5.5 to 8.7 s. **A person watched it, and `prompt-and-reply.mp4` and `quick-chat.mp4` both played correctly (reported by the user, 2026-11-26).** Before that, frames cut from the MP4 at 1.0, 4.5 and 9.0 s were looked at by the implementing agent: the title card ("Ask, and watch it stream", "QueryMT Desktop", accent rule), the framed footage (title bar with three dots, streaming reply), and the end card.   - The knowledge note's example path now points at `demos/clips/prompt-and-reply/scene.toml`.

## 3. Cursor, zoom, captions and panel

- [x] 2.4-fix (after review, 2026-11-26): the first build drew a separate 44 px title bar with three dots and a `window_title` above the footage. The app has no title bar of its own (`appears_transparent` and `traffic_light_position` (14, 15) in `src/ui/mod.rs`), so the video showed two bars. The bar and the `window_title` variable are removed (template, README, `quick-chat/clip.json`, `design.md`, `proposal.md`). The three buttons are now drawn over the footage at (14, 15), 12 px wide and 20 px apart, scaled with the footage; the window radius is 10 px. Both clips were re-rendered with `demos/film.sh --no-film --all` (5.1 s and 7.4 s) and `check` passes on both. Frames at 4.5 s and 5.0 s show the buttons left of the logo, no second bar, and the panel's rounded corners unchanged.
- [x] 3.1 Cursor and click ring from `QMT_FILM.events` (design D4), with positions from `pct` mapped into the frame's screen rect.
  - Verify:
    - `npx --yes hyperframes@0.9.98 snapshot target/demos/prompt-and-reply --at <t of the click event>,<t of the type event + 0.3>` (shape per 1.2) shows the cursor on the composer;
    - `npx --yes hyperframes@0.9.98 check target/demos/prompt-and-reply` reports no errors.
  - Recorded: `prompt-and-reply` has no click events (its prompt is scripted), so the cursor was checked on `quick-chat`, whose first step is a `click = "composer-input"` at 0.4 s. `snapshot target/demos/quick-chat --at 2.95,3.8` wrote both PNGs, and frames from the MP4 show the arrow cursor over the composer. `check` on both clips: 0 errors. It first reported one error on `prompt-and-reply` (`text_occluded`: the window title is under the opaque title card on purpose), fixed with `data-layout-allow-occlusion data-layout-ignore` on `#win-title`.
  - The panel's typing has no `pct` (the harness finds no `composer-input` in the panel), so typing there moves no cursor and does not trigger a zoom. Noted in the README.
- [x] 3.2 Zoom (`zoomPlan`) and captions. Write `zoomPlan` as a pure function in a `<script>` block of the template, and test it from `demos/build.test.mjs` by extracting that block with a marker comment (a sequence of events produces the expected push, hold and return).
  - Verify:
    - `node --test demos/build.test.mjs` passes;
    - snapshots at a zoomed moment and during a caption show both;
    - `check` reports no errors.
  - Recorded: `node --test` 19 pass, of which 9 cover `zoomPlan`, `zoomTriggers` and `mainPoint` (one click: push at t-0.6, hold, return at +1.2 s, 0.6 s long; a near event extends the hold; typing holds to `until`; a far event returns before the next push). A frame of `quick-chat` at 2.7 s shows the zoomed window, and at 4.0 s a zoomed window with the caption "Summon the quick panel from anywhere" visible. `check` 0 errors.
- [x] 3.3 Panel layer: the `<video>` of `assets/panel.webm`, anchored with `bounds` placement, `visible` ranges, shadow and backdrop blur (design D4).
  - Add `demos/clips/quick-chat/{scene.toml,clip.json}` (design D5), and run `demos/film.sh quick-chat`.
  - Verify:
    - `target/demos/quick-chat.mp4` renders;
    - a snapshot on the panel's first visible frame shows rounded panel corners with the main footage behind them, not black (the alpha check from the design's Risks);
    - if the corners are black, set `panel = "composite"` in the scene, record that, and re-verify;
    - record whether the backdrop blur shows, and the render time.
  - Recorded: `demos/film.sh quick-chat` wrote `target/demos/quick-chat.mp4` (`1920,1080,30/1`, 14.0 s; film 44 s, render 8.6 to 9.5 s). Frames at 3.75 s and 5.0 s show the panel with **rounded corners and the main footage behind them, not black**, so the WebM alpha is honoured and `panel = "composite"` is not needed. The backdrop blur (18 px, glass alpha 0.18) is **not visibly distinguishable** over the mostly flat dark footage here, so it is kept but not claimed. The glass radius (14 px) is a guess at the panel's own.
  - The scene is `smoke-panel` plus an open session, a first click, and captions. The hand-off uses `press = "cmd-enter"` in the panel (`submit_and_open`; the panel has no element id for an open button). The panel closes at frame 107 and the main window shows the new session with the question and the streamed answer (checked on `still-239.png` and on MP4 frames at 6.6 s and 9.5 s). Final render: 14.0 s, `1920,1080,30/1`, 7.2 s render. The frame checks above were made on the earlier Enter-only version of this scene; the corners and glass are the same layer, and the final scene was re-rendered, not re-inspected at the panel's first frame.

## 4. Docs

- [x] 4.1 Write `demos/README.md`:
  - prerequisites (Node ≥ 22, ffmpeg with VP9, cargo-nextest; the first `npx` run downloads HyperFrames and Chrome);
  - `film.sh` usage, including `--no-film` for composition-only changes;
  - how to add a clip (copy a clip folder, edit `scene.toml` and `clip.json`);
  - the composition variables table;
  - `npx hyperframes@0.9.98 preview target/demos/<clip>` for looking around;
  - where outputs go;
  - the pinned version and how to bump it.
  - Verify: following the README from "add a clip" produces a third throwaway clip end to end. Delete it afterwards and record the run.
  - Recorded: `cp -r demos/clips/quick-chat demos/clips/my-clip`, set `name = "my-clip"`, `demos/film.sh my-clip` (58.6 s, exit 0) wrote `target/demos/my-clip.mp4`, `1920,1080,30/1`, 14.0 s. Deleted the clip, its project and its footage afterwards.
- [x] 4.2 Add a "Demo videos" pointer to `demos/README.md` from the filming section of `crates/querymt-gpui/knowledge/development.md`, and add `demos` to that note's `knowledge_paths`.
  - Verify: `cargo run -p querymt-agent --example repository_knowledge -- --root . check` reports nothing new for this note.
  - Recorded: the check reports nothing for `crates/querymt-gpui/knowledge/development.md` (the other warnings are about `agent-runtime` notes and rustc dead-code warnings, none from this change). `knowledge_paths` lists `demos/README.md`, `demos/film.sh`, `demos/build.mjs` and `demos/clips/*/scene.toml` instead of `demos`: a bare `demos` matched no indexed file (warning `knowledge_path_unmatched`). Those four are selected in `.qmt/knowledge.toml`, a file this change did not plan to touch.

## 5. Final checks

- [x] 5.1 `demos/film.sh --all` renders both clips from a clean `target/film` and `target/demos`. Record the total wall time.
  - Recorded: after `rm -rf target/film target/demos`, `demos/film.sh --all` exited 0 in **1 m 35 s** (real). `prompt-and-reply` 10.0 s, 585 KB, render 5.5 s; `quick-chat` 14.0 s, 1.7 MB, render 8.8 s.
- [x] 5.2 `git status` shows no files from the renders outside `target/`.
  - Recorded: `git status --short` lists only `demos/` sources, the moved scene, `.qmt/knowledge.toml` and `crates/querymt-gpui/knowledge/development.md`. A modified root `.DS_Store` and a `demos/clips/.DS_Store` come from Finder, not the renders (the second was deleted).
- [x] 5.3 `openspec validate add-gpui-demo-videos --strict` passes.
  - Recorded: "Change 'add-gpui-demo-videos' is valid". Also: `node --test demos/build.test.mjs` 19 pass, `shellcheck demos/film.sh` clean, `cargo nextest run -p querymt-gpui --bins -E 'test(/film::/)'` 27 passed (the scene move broke nothing).

## Workflow follow-up

- Archive this change after review.
- If the jsDelivr GSAP dependency causes failed renders, vendor GSAP into `demos/shared/` in a follow-up.
