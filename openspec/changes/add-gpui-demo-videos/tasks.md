# Tasks

This change sets `skip_specs: true` (developer tooling and media templates), so no task covers spec scenarios. It needs `add-gpui-film-scenes` applied first: the film test `film::film_scene`, the timing file and `panel.webm`. Commands run from the repository root. Findings are recorded under their task, with the date, the machine and the exact command.

## 1. Prerequisites and CLI shapes

- [ ] 1.1 Confirm `add-gpui-film-scenes` is applied:

  ```bash
  QMT_FILM_SCENE=crates/querymt-gpui/film/scenes/prompt-and-reply.toml cargo nextest run -p querymt-gpui --bins --run-ignored only -E 'test(=film::film_scene)' --no-capture
  ```

  This should write `target/film/prompt-and-reply/{main.mp4,prompt-and-reply.film.json,film.js}`.
  - Verify: the command passes and the three files exist.
- [ ] 1.2 Run `npx --yes hyperframes@0.9.98 doctor`, and record from `--help` for `render`, `lint`, `check` and `snapshot`:
  - whether a project directory is passed positionally or through the working directory;
  - the exact `--variables-file`, `--output` and `--quality` spellings.
  - Verify: the doctor result and the four argument shapes are recorded below this task. D2 of the design follows them.

## 2. Project assembly

- [ ] 2.1 Add `demos/shared/hyperframes.json` (settings as `npx hyperframes@0.9.98 init` writes them for a blank project; copy that file's shape, not invented keys) and `demos/shared/app-clip.html`, a first version containing:
  - the root with the `{{DURATION}}`, `{{FPS}}`, `{{FOOTAGE_W}}` and `{{FOOTAGE_H}}` placeholders;
  - the variables from design D4;
  - the background, the window frame, and the main footage clip;
  - title and end cards;
  - a paused GSAP timeline registered as `window.__timelines["app-clip"]`.

  No cursor, zoom, panel or captions yet.
  - Verify: done together with 2.2.
- [ ] 2.2 Add `demos/build.mjs` (design D3) and `demos/build.test.mjs`.
  - Verify:
    - `node --test demos/build.test.mjs` passes;
    - `node demos/build.mjs prompt-and-reply` writes `target/demos/prompt-and-reply/` and prints its duration;
    - `npx --yes hyperframes@0.9.98 lint target/demos/prompt-and-reply` reports no errors.
- [ ] 2.3 Add `demos/film.sh` (design D2): prerequisite checks, `--no-film`, `--all`, `HYPERFRAMES` and `QMT_FILM_OUT`; mark it executable.
  - Verify:
    - `bash -n demos/film.sh` passes, and `shellcheck demos/film.sh` passes if installed;
    - `demos/film.sh` with no arguments prints usage and exits 2;
    - with `PATH` lacking `node`, it exits 1 naming Node 22.
- [ ] 2.4 Move `crates/querymt-gpui/film/scenes/prompt-and-reply.toml` to `demos/clips/prompt-and-reply/scene.toml`, and add its `clip.json` (design D5). Update the film knowledge note's example path.
  - Verify:
    - `demos/film.sh prompt-and-reply` writes `target/demos/prompt-and-reply.mp4`;
    - `ffprobe -v error -show_entries stream=width,height,r_frame_rate:format=duration -of csv=p=0 target/demos/prompt-and-reply.mp4` reports `1920,1080,30/1` and a duration equal to the printed one;
    - a person watches it once (title card, framed footage, end card) and the result is recorded.

## 3. Cursor, zoom, captions and panel

- [ ] 3.1 Cursor and click ring from `QMT_FILM.events` (design D4), with positions from `pct` mapped into the frame's screen rect.
  - Verify:
    - `npx --yes hyperframes@0.9.98 snapshot target/demos/prompt-and-reply --at <t of the click event>,<t of the type event + 0.3>` (shape per 1.2) shows the cursor on the composer;
    - `npx --yes hyperframes@0.9.98 check target/demos/prompt-and-reply` reports no errors.
- [ ] 3.2 Zoom (`zoomPlan`) and captions. Write `zoomPlan` as a pure function in a `<script>` block of the template, and test it from `demos/build.test.mjs` by extracting that block with a marker comment (a sequence of events produces the expected push, hold and return).
  - Verify:
    - `node --test demos/build.test.mjs` passes;
    - snapshots at a zoomed moment and during a caption show both;
    - `check` reports no errors.
- [ ] 3.3 Panel layer: the `<video>` of `assets/panel.webm`, anchored with `bounds` placement, `visible` ranges, shadow and backdrop blur (design D4).
  - Add `demos/clips/quick-chat/{scene.toml,clip.json}` (design D5), and run `demos/film.sh quick-chat`.
  - Verify:
    - `target/demos/quick-chat.mp4` renders;
    - a snapshot on the panel's first visible frame shows rounded panel corners with the main footage behind them, not black (the alpha check from the design's Risks);
    - if the corners are black, set `panel = "composite"` in the scene, record that, and re-verify;
    - record whether the backdrop blur shows, and the render time.

## 4. Docs

- [ ] 4.1 Write `demos/README.md`:
  - prerequisites (Node ≥ 22, ffmpeg with VP9, cargo-nextest; the first `npx` run downloads HyperFrames and Chrome);
  - `film.sh` usage, including `--no-film` for composition-only changes;
  - how to add a clip (copy a clip folder, edit `scene.toml` and `clip.json`);
  - the composition variables table;
  - `npx hyperframes@0.9.98 preview target/demos/<clip>` for looking around;
  - where outputs go;
  - the pinned version and how to bump it.
  - Verify: following the README from "add a clip" produces a third throwaway clip end to end. Delete it afterwards and record the run.
- [ ] 4.2 Add a "Demo videos" pointer to `demos/README.md` from the filming section of `crates/querymt-gpui/knowledge/development.md`, and add `demos` to that note's `knowledge_paths`.
  - Verify: `cargo run -p querymt-agent --example repository_knowledge -- --root . check` reports nothing new for this note.

## 5. Final checks

- [ ] 5.1 `demos/film.sh --all` renders both clips from a clean `target/film` and `target/demos`. Record the total wall time.
- [ ] 5.2 `git status` shows no files from the renders outside `target/`.
- [ ] 5.3 `openspec validate add-gpui-demo-videos --strict` passes.

## Workflow follow-up

- Archive this change after review.
- If the jsDelivr GSAP dependency causes failed renders, vendor GSAP into `demos/shared/` in a follow-up.
