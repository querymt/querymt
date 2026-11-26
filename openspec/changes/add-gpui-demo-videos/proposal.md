# Proposal

## Why

Change `add-gpui-film-scenes` makes clips of QueryMT Desktop from scene files. Its outputs are raw footage (`main.mp4`, optionally a transparent `panel.webm`) and a timing file of clicks, keys, marks and captions. Raw footage is not a demo video yet: it has no window frame, no cursor, no zoom on what matters, no captions or title.

HyperFrames (HTML compositions rendered deterministically to video, CLI `npx hyperframes`) can add all of that from the timing file. A UI change then means one command per clip, not a re-edit.

## What Changes

- **`demos/` tree** at the repository root:
  - `demos/clips/<clip>/scene.toml`: the scene, in `add-gpui-film-scenes` format;
  - `demos/clips/<clip>/clip.json`: the clip's HyperFrames variables (title, subtitle, window title, zoom strength, cursor on or off);
  - `demos/shared/`: a shared HyperFrames composition template, `app-clip.html`, and the project settings file `hyperframes.json`;
  - `demos/build.mjs`: a dependency-free Node script that assembles a HyperFrames project for one clip, with tests in `demos/build.test.mjs` (`node --test`);
  - `demos/film.sh`: the one entry point;
  - `demos/README.md`.
- **Shared composition `app-clip.html`**, a deterministic, paused GSAP timeline. GSAP 3 loads from jsDelivr before frame 0, as HyperFrames' own docs do; there are no other network reads. It has:
  - a 1920x1080 stage, with a macOS-style window frame drawn around the footage (patterned on the catalog's `browser-device-stage` "window" chrome);
  - the main footage;
  - the transparent panel layer, placed by the timing file's anchor and bounds, with a soft shadow and a backdrop blur;
  - a cursor that moves between the timing file's event points, with a click ring on clicks and an I-beam while typing;
  - zoom pushes on events and marks;
  - captions from the timing file;
  - an optional title card and end card.

  Everything is driven by `window.QMT_FILM` (`film.js`) and the clip's variables.
- **`demos/film.sh <clip>…`:**
  1. films each scene with the `add-gpui-film-scenes` test;
  2. runs `build.mjs`, which writes a HyperFrames project to `target/demos/<clip>/`: `index.html` from the template, sized from the timing file, plus `assets/` holding the footage and `film.js`;
  3. runs `npx hyperframes@0.9.98 lint`;
  4. runs `npx hyperframes@0.9.98 render --variables-file demos/clips/<clip>/clip.json --output target/demos/<clip>.mp4`.

  `--all` does every clip. `--no-film` re-renders from existing footage, for composition-only changes; this is the fast path when only the video side changes.
- **The HyperFrames version is pinned on the command line** (`npx hyperframes@0.9.98`, the current release). There is no `package.json` or lockfile in the repository. The version is one variable at the top of `film.sh`.
- **Two starter clips:**
  - `prompt-and-reply`: the spike's clip, moved from `crates/querymt-gpui/film/scenes/`;
  - `quick-chat`: the quick panel opened over a session, a short chat, then handed off to the main window.
- **Docs:**
  - `demos/README.md`: prerequisites, the commands, how to add a clip, and the composition's variables;
  - a pointer from the "Filming demo clips" section of `crates/querymt-gpui/knowledge/development.md`.

No Rust code changes in this change, and no product behaviour, API, config, wire shape or schema changes.

## Non-goals

- The film harness itself, the scene format and the timing file: change `add-gpui-film-scenes`.
- Voiceover, music, sound effects, avatars, or HyperFrames cloud or Studio rendering.
- Rendering in CI, or committing rendered videos. Outputs live under `target/`.
- Docker renders for cross-machine pixel reproducibility (`--docker`). Same-machine renders are deterministic enough for demos.
- A `package.json` or lockfile for HyperFrames (pinned via `npx hyperframes@<version>` instead).
- Installing HyperFrames agent skills into this repository.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. This adds developer tooling and media templates, with no system behaviour change, so the change sets `skip_specs: true`.

## Impact

- **Packages:** none. No Cargo package changes.
- **Repository files:**
  - new `demos/` (shell script, Node script, HTML template, JSON, TOML, README);
  - scenes move from `crates/querymt-gpui/film/scenes/` to `demos/clips/*/scene.toml` (`smoke*.toml` stays with the harness);
  - `crates/querymt-gpui/knowledge/development.md` gets a pointer.
- **Tools** (only for making demos):
  - Node 22 or newer;
  - `npx`, which downloads `hyperframes@0.9.98` and its headless Chrome on first use;
  - `ffmpeg` with `libvpx-vp9`;
  - everything the film harness needs.
- **Outputs:** `target/demos/` (already ignored by `/target`). No `.gitignore` change is needed.
- **CI and special builds:** unchanged.
- **querymt-desktop:** no UI follow-up.
