// node --test demos/build.test.mjs
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { END_S, TITLE_S, build, checkTiming, fill, placeholders } from "./build.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));

function timing(over = {}) {
  return {
    version: 1,
    name: "demo",
    fps: 30,
    frames: 150,
    scale: 2,
    canvas: { width: 1280, height: 800 },
    tracks: [{ window: "main", file: "main.mp4", size: [1280, 800], visible: [[0, 149]], bounds: [[0, 0, 0, 1280, 800]] }],
    events: [],
    marks: [],
    captions: [],
    ...over,
  };
}

/** A temporary film folder holding a timing file and its track files. */
function fixture(t, tm = timing(), files = ["main.mp4", "film.js"]) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "demos-build-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const film = path.join(dir, "film", tm.name);
  fs.mkdirSync(film, { recursive: true });
  fs.writeFileSync(path.join(film, `${tm.name}.film.json`), JSON.stringify(tm));
  for (const f of files) fs.writeFileSync(path.join(film, f), "x");
  return { dir, film, out: path.join(dir, "out", tm.name) };
}

test("fill replaces every placeholder, including repeats", () => {
  assert.equal(fill("a {{X}} b {{Y}} {{X}}", { X: 1, Y: "two" }), "a 1 b two 1");
});

test("fill fails on an unknown placeholder, naming it", () => {
  assert.throws(() => fill("{{NOPE}}", { X: 1 }), /\{\{NOPE\}\}/);
});

test("placeholders add the cards to the footage length", () => {
  const p = placeholders(timing());
  assert.equal(p.DURATION, TITLE_S + 5 + END_S);
  assert.equal(p.FPS, 30);
  assert.equal(p.FOOTAGE_W, 1280);
  assert.equal(p.FOOTAGE_H, 800);
  assert.equal(p.FOOTAGE_S, 5);
  assert.equal(p.FOOTAGE_AT, TITLE_S);
  assert.equal(p.END_AT, TITLE_S + 5);
});

test("checkTiming rejects another version", (t) => {
  const { film } = fixture(t);
  assert.throws(() => checkTiming(timing({ version: 2 }), "demo", film), /version 2/);
});

test("checkTiming rejects a scene whose name is not the clip", (t) => {
  const { film } = fixture(t);
  assert.throws(() => checkTiming(timing({ name: "other" }), "demo", film), /"other".*"demo"/);
});

test("checkTiming reports a missing track file", (t) => {
  const { film } = fixture(t, timing(), ["film.js"]);
  assert.throws(() => checkTiming(timing(), "demo", film), /main\.mp4/);
});

test("build writes the project and leaves no placeholder behind", (t) => {
  const { film, out } = fixture(t);
  const r = build("demo", { filmDir: film, outDir: out, stills: false });
  assert.equal(r.duration, 10);
  const html = fs.readFileSync(path.join(out, "index.html"), "utf8");
  assert.doesNotMatch(html, /\{\{[A-Za-z_]+\}\}/);
  assert.match(html, /data-duration="10"/);
  assert.match(html, /data-fps="30"/);
  assert.match(html, /var FW = 1280;/);
  for (const f of ["hyperframes.json", "assets/main.mp4", "assets/film.js"]) {
    assert.ok(fs.existsSync(path.join(out, f)), f);
  }
});

test("build copies the panel track when the timing lists one", (t) => {
  const tm = timing({
    tracks: [
      ...timing().tracks,
      { window: "panel", file: "panel.webm", size: [680, 760], anchor: { x: 0.5, bottom: 0.15 }, visible: [[15, 90]], bounds: [[15, 0, 696, 680, 64]] },
    ],
  });
  const { film, out } = fixture(t, tm, ["main.mp4", "panel.webm", "film.js"]);
  build("demo", { filmDir: film, outDir: out, stills: false });
  assert.ok(fs.existsSync(path.join(out, "assets/panel.webm")));
  const html = fs.readFileSync(path.join(out, "index.html"), "utf8");
  assert.match(html, /<video id="panel-video"[^>]*src="assets\/panel\.webm"/);
});

test("build leaves the panel video out when there is no panel track", (t) => {
  const { film, out } = fixture(t);
  build("demo", { filmDir: film, outDir: out, stills: false });
  assert.doesNotMatch(fs.readFileSync(path.join(out, "index.html"), "utf8"), /<video id="panel-video"/);
});

test("build without a timing file says how to make one", (t) => {
  const { dir } = fixture(t);
  assert.throws(() => build("nope", { filmDir: path.join(dir, "film/nope"), outDir: path.join(dir, "o"), stills: false }), /film the clip first/);
});

// --- zoomPlan: cut out of the template between its marker comments.

const template = fs.readFileSync(path.join(here, "shared/app-clip.html"), "utf8");
const block = /\/\* zoomPlan:start \*\/([\s\S]*?)\/\* zoomPlan:end \*\//.exec(template);
assert.ok(block, "the template has a zoomPlan block");
const { zoomPlan, zoomTriggers, mainPoint } = new Function(`${block[1]}; return { zoomPlan, zoomTriggers, mainPoint };`)();

const near = (a, b) => assert.ok(Math.abs(a - b) < 1e-9, `${a} != ${b}`);

test("zoomPlan: one click pushes, holds and returns", () => {
  const [seg, ...rest] = zoomPlan([{ t: 2, x: 50, y: 80 }]);
  assert.equal(rest.length, 0);
  near(seg.pushStart, 1.4); // 0.6 s before the click
  near(seg.pushEnd, 2); // fully in at the click
  near(seg.returnStart, 3.2); // 1.2 s with nothing new
  near(seg.returnEnd, 3.8);
});

test("zoomPlan: a push never starts before the clip does", () => {
  const [seg] = zoomPlan([{ t: 0.2, x: 10, y: 10 }]);
  near(seg.pushStart, 0);
  assert.ok(seg.pushEnd > seg.pushStart);
});

test("zoomPlan: a nearby event within the quiet window extends the hold", () => {
  const segs = zoomPlan([
    { t: 2, x: 50, y: 80 },
    { t: 3, x: 60, y: 70 },
  ]);
  assert.equal(segs.length, 1);
  assert.equal(segs[0].points.length, 2);
  near(segs[0].returnStart, 4.2);
});

test("zoomPlan: typing holds the zoom until it ends", () => {
  const [seg] = zoomPlan([{ t: 1, x: 40, y: 85, until: 4 }]);
  near(seg.returnStart, 5.2);
});

test("zoomPlan: events more than 1.2 s apart are separate pushes", () => {
  const segs = zoomPlan([
    { t: 2, x: 50, y: 50 },
    { t: 6, x: 52, y: 50 },
  ]);
  assert.equal(segs.length, 2);
  near(segs[0].returnEnd, 3.8);
  near(segs[1].pushStart, 5.4);
});

test("zoomPlan: a far event returns first, then pushes again", () => {
  const segs = zoomPlan([
    { t: 2, x: 10, y: 10 },
    { t: 3, x: 90, y: 90 },
  ]);
  assert.equal(segs.length, 2);
  assert.ok(segs[0].returnEnd <= segs[1].pushStart + 1e-9, "segments do not overlap");
  assert.ok(segs[1].pushEnd >= 3 - 1e-9);
});

test("zoomPlan: no triggers, no zoom", () => {
  assert.deepEqual(zoomPlan([]), []);
});

test("zoomTriggers: marks, clicks and typing count; the rest do not", () => {
  const film = {
    fps: 30,
    canvas: { width: 1280, height: 800 },
    tracks: [{ window: "main", file: "main.mp4", size: [1280, 800] }],
    marks: [{ name: "m", frame: 60, window: "main", pct: [40, 40, 20, 20] }],
    events: [
      { t: 1, kind: "click", window: "main", pct: [10, 20, 10, 10] },
      { t: 2, kind: "type", window: "main", pct: [10, 80, 40, 10], until: 3 },
      { t: 3, kind: "stream", window: "main" },
      { t: 4, kind: "press", window: "main" },
    ],
  };
  const tr = zoomTriggers(film).sort((a, b) => a.t - b.t);
  assert.equal(tr.length, 3);
  assert.deepEqual(tr.map((x) => x.t), [1, 2, 2]); // the click, then the mark (frame 60) and the typing
  near(tr[0].x, 15); // click: the centre of its rect
  near(tr[0].y, 25);
  const mark = tr.find((x) => x.until === undefined && x.t === 2);
  near(mark.x, 50);
  near(mark.y, 50);
  const typing = tr.find((x) => x.until === 3);
  near(typing.x, 10); // type: the text start, not the field's centre
  near(typing.y, 85);
});

test("mainPoint maps a panel event through the anchor into the main frame", () => {
  const film = {
    canvas: { width: 1280, height: 800 },
    tracks: [
      { window: "main", file: "main.mp4", size: [1280, 800] },
      { window: "panel", file: "panel.webm", size: [680, 760], anchor: { x: 0.5, bottom: 0.15 } },
    ],
  };
  // The panel canvas's centre is the anchor's x; its bottom edge is 15% up.
  const p = mainPoint(film, "panel", [50, 100, 0, 0], false);
  near(p.x, 50);
  near(p.y, 85);
});
