#!/usr/bin/env node
// Assembles one HyperFrames project for a filmed clip (change add-gpui-demo-videos, design D3).
//
//   node demos/build.mjs <clip>
//
// Reads  $QMT_FILM_OUT/<clip>/<clip>.film.json   (default target/film/)
// Writes $QMT_DEMOS_OUT/<clip>/                  (default target/demos/)
//   index.html, hyperframes.json, assets/{main.mp4, panel.webm?, film.js, first.png, last.png}
//
// Plain Node ESM, `fs` and `path` only; `ffmpeg` is run (when present) for the two stills.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");

/** Length of the title and end cards, in seconds. Fixed, so the root's duration needs no variables. */
export const TITLE_S = 2.5;
export const END_S = 2.5;

/** Replace every `{{NAME}}` in `template`; an unknown or unfilled name is an error. */
export function fill(template, values) {
  return template.replace(/\{\{([A-Za-z_][A-Za-z0-9_]*)\}\}/g, (_, name) => {
    if (!Object.hasOwn(values, name)) {
      throw new Error(`unknown placeholder {{${name}}} in the template`);
    }
    return String(values[name]);
  });
}

/** Round to milliseconds, so the HTML carries no float noise. */
const sec = (v) => Math.round(v * 1000) / 1000;

/** The panel's `<video>`, or an empty string when no track of the panel was filmed. */
function panelVideo(timing, footage) {
  const panel = timing.tracks.find((t) => t.window === "panel" && t.file);
  if (!panel) return "";
  return (
    `<video id="panel-video" class="clip" src="assets/${panel.file}" ` +
    `data-start="${TITLE_S}" data-duration="${sec(footage)}" data-track-index="3" muted playsinline></video>`
  );
}

/** The placeholder values for a timing file. */
export function placeholders(timing) {
  const footage = timing.frames / timing.fps;
  return {
    DURATION: sec(TITLE_S + footage + END_S),
    FPS: timing.fps,
    FOOTAGE_W: timing.canvas.width,
    FOOTAGE_H: timing.canvas.height,
    FOOTAGE_S: sec(footage),
    FOOTAGE_AT: TITLE_S,
    END_AT: sec(TITLE_S + footage),
    END_S,
    PANEL_VIDEO: panelVideo(timing, footage),
  };
}

/** Throws unless `timing` is a version 1 timing file of `clip` whose track files all exist in `dir`. */
export function checkTiming(timing, clip, dir) {
  if (timing.version !== 1) {
    throw new Error(`timing file has version ${timing.version}, expected 1`);
  }
  if (timing.name !== clip) {
    throw new Error(`timing file is for "${timing.name}", expected "${clip}" (the scene's name must equal the clip folder)`);
  }
  for (const track of timing.tracks) {
    if (track.file && !fs.existsSync(path.join(dir, track.file))) {
      throw new Error(`track file ${track.file} (window ${track.window}) is missing from ${dir}`);
    }
  }
}

function still(args, out) {
  const r = spawnSync("ffmpeg", ["-v", "error", "-y", ...args, out], { encoding: "utf8" });
  if (r.error || r.status !== 0 || !fs.existsSync(out)) {
    throw new Error(`ffmpeg could not write ${out}: ${r.error?.message ?? r.stderr}`);
  }
}

/**
 * Build the project. `filmDir` holds the film's outputs, `outDir` is the project folder to write.
 * `stills: false` skips the two ffmpeg stills (the tests have no real footage).
 * Returns `{ outDir, duration }`.
 */
export function build(clip, { filmDir, outDir, templateDir = path.join(here, "shared"), stills = true }) {
  const timingFile = path.join(filmDir, `${clip}.film.json`);
  if (!fs.existsSync(timingFile)) {
    throw new Error(`no timing file ${timingFile}; film the clip first (demos/film.sh ${clip})`);
  }
  const timing = JSON.parse(fs.readFileSync(timingFile, "utf8"));
  checkTiming(timing, clip, filmDir);

  fs.rmSync(outDir, { recursive: true, force: true });
  const assets = path.join(outDir, "assets");
  fs.mkdirSync(assets, { recursive: true });
  for (const track of timing.tracks) {
    if (track.file) fs.copyFileSync(path.join(filmDir, track.file), path.join(assets, track.file));
  }
  fs.copyFileSync(path.join(filmDir, "film.js"), path.join(assets, "film.js"));
  fs.copyFileSync(path.join(templateDir, "hyperframes.json"), path.join(outDir, "hyperframes.json"));

  const values = placeholders(timing);
  const html = fill(fs.readFileSync(path.join(templateDir, "app-clip.html"), "utf8"), values);
  fs.writeFileSync(path.join(outDir, "index.html"), html);

  if (stills) {
    // The cards hide the footage; a held first and last frame sit under them.
    const main = path.join(assets, "main.mp4");
    still(["-i", main, "-frames:v", "1"], path.join(assets, "first.png"));
    still(["-sseof", "-0.5", "-i", main, "-update", "1"], path.join(assets, "last.png"));
  }
  return { outDir, duration: values.DURATION };
}

function main(argv) {
  const clip = argv[0];
  if (!clip || argv.length !== 1 || clip.startsWith("-")) {
    console.error("usage: node demos/build.mjs <clip>");
    return 2;
  }
  const filmBase = process.env.QMT_FILM_OUT ? path.resolve(process.env.QMT_FILM_OUT) : path.join(root, "target/film");
  const outBase = process.env.QMT_DEMOS_OUT ? path.resolve(process.env.QMT_DEMOS_OUT) : path.join(root, "target/demos");
  try {
    const { outDir, duration } = build(clip, { filmDir: path.join(filmBase, clip), outDir: path.join(outBase, clip) });
    console.log(`project: ${path.relative(root, outDir)}`);
    console.log(`duration: ${duration}`);
    return 0;
  } catch (e) {
    console.error(`build: ${e.message}`);
    return 1;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main(process.argv.slice(2)));
}
