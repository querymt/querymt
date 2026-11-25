//! Paints a limu `Scene` into a GPUI window and measures text with GPUI's text
//! system.
//!
//! Ported from `limu-gpui` 0.2.1 (MIT, Copyright (c) Firede,
//! <https://github.com/firede/limu-rs>), which does not build against the
//! `gpui-pre` 0.3.14 that gpui-kit 0.11 pins: that release dropped lyon's
//! `FillOptions`/`StrokeOptions` for its own (design D6 of
//! `render-mermaid-diagrams`). Differences from the original: GPUI's own
//! path style types, no icon or image support (diagrams here carry neither),
//! and no recording canvas for tests.
//!
//! Paths are filled by the nonzero rule and stroked with SVG's caps, joins and
//! miter limit. Upright text is painted glyph by glyph on the run's baseline.
//! Turned text and halos are written as SVG text (see `svg.rs`), because GPUI
//! paints a glyph only upright and filled.

use std::hash::{Hash, Hasher};
use std::rc::Rc;

use gpui_kit as gpui;
use gpui_kit::{
    App, Bounds, Font, FontStyle, FontWeight, Hsla, PathBuilder, PathStyle, Pixels, Rgba,
    SharedString, TextRun, TransformationMatrix, Window, WindowTextSystem, point, px, radians,
    size,
};
use limu::scene::style::{Cap, Ink, Join, Look, Resolved};
use limu::scene::text::{Face, Measurer};
use limu::scene::{Align, Canvas, Item, Painter, Pen, Point, Rect, Run, Scene, Step, TextPaint};

use super::svg;

/// The families text is set in: one for text, one for monospaced text. Bold and
/// italic text uses the family's bold and italic faces.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Fonts {
    pub(crate) sans: SharedString,
    pub(crate) mono: SharedString,
}

impl Fonts {
    fn font(&self, face: Face) -> Font {
        let family = if face.contains(Face::MONO) {
            &self.mono
        } else {
            &self.sans
        };
        let mut f = gpui::font(family.clone());
        if face.contains(Face::BOLD) {
            f.weight = FontWeight::BOLD;
        }
        if face.contains(Face::ITALIC) {
            f.style = FontStyle::Italic;
        }
        f
    }
}

/// Measures text as a window's text system sets it, so a scene laid out with it
/// is drawn in the faces it was measured in.
#[derive(Clone)]
pub(crate) struct TextMeasurer {
    system: Rc<WindowTextSystem>,
    fonts: Fonts,
}

impl TextMeasurer {
    pub(crate) fn new(window: &Window, fonts: Fonts) -> Self {
        Self {
            system: window.text_system().clone(),
            fonts,
        }
    }
}

impl Measurer for TextMeasurer {
    fn width(&self, s: &str, size: f64, f: Face) -> f64 {
        let run = TextRun {
            len: s.len(),
            font: self.fonts.font(f),
            ..TextRun::default()
        };
        f64::from(
            self.system
                .layout_line(s, px(size as f32), &[run], None)
                .width,
        )
    }
}

/// Paints a scene in a window, its origin at a point and each of its units
/// `scale` pixels. The scene's background is left to the host.
pub(crate) fn paint(
    scene: &Scene,
    look: &Look,
    fonts: &Fonts,
    origin: gpui::Point<Pixels>,
    scale: f32,
    window: &mut Window,
    cx: &mut App,
) {
    let base = Affine::place(f64::from(origin.x), f64::from(origin.y), f64::from(scale));
    let mut canvas = GpuiCanvas {
        window,
        cx,
        fonts,
        transforms: vec![base],
    };
    limu::scene::draw_with(
        scene,
        look,
        &mut canvas,
        |p: &mut Painter<'_, GpuiCanvas<'_>>, it: &Item, s: &Resolved| p.item(it, s),
    );
}

struct GpuiCanvas<'a> {
    window: &'a mut Window,
    cx: &'a mut App,
    fonts: &'a Fonts,
    // From the scene's units to the window's pixels, the last for what is drawn now.
    transforms: Vec<Affine>,
}

impl GpuiCanvas<'_> {
    fn at(&self) -> Affine {
        *self.transforms.last().expect("a transform")
    }

    fn fill(&mut self, path: &limu::scene::Path, ink: &Ink) {
        let t = self.at();
        let mut b = PathBuilder::fill().with_style(PathStyle::Fill(gpui::FillOptions {
            fill_rule: gpui::FillRule::NonZero,
        }));
        trace(&mut b, path, t);
        if let Ok(p) = b.build() {
            self.window.paint_path(p, hsla(ink));
        }
    }

    fn stroke(
        &mut self,
        path: &limu::scene::Path,
        ink: &Ink,
        width: f64,
        cap: Cap,
        join: Join,
        dash: &[f64],
    ) {
        let t = self.at();
        let k = t.scale();
        let w = width * k;
        let dash: Vec<f64> = dash_of(dash).iter().map(|d| d * k).collect();
        let mut b =
            PathBuilder::stroke(px(w as f32)).with_style(PathStyle::Stroke(gpui::StrokeOptions {
                line_width: w as f32,
                line_cap: match cap {
                    Cap::Butt => gpui::LineCap::Butt,
                    Cap::Round => gpui::LineCap::Round,
                    Cap::Square => gpui::LineCap::Square,
                },
                line_join: match join {
                    Join::Miter => gpui::LineJoin::Miter,
                    Join::Round => gpui::LineJoin::Round,
                    Join::Bevel => gpui::LineJoin::Bevel,
                },
                // GPUI 0.3.14 strokes with tiny-skia, whose miter limit is
                // SVG's, so the scene's value applies as is.
                miter_limit: limu::scene::MITER_LIMIT as f32,
            }));
        if !dash.is_empty() {
            let d: Vec<Pixels> = dash.iter().map(|&d| px(d as f32)).collect();
            b = b.dash_array(&d);
        }
        trace(&mut b, path, t);
        if let Ok(p) = b.build() {
            self.window.paint_path(p, hsla(ink));
        }
    }

    /// Paints a run upright and filled, glyph by glyph on its baseline.
    fn glyphs(&mut self, run: &Run<'_>, ink: &Ink) {
        let t = self.at();
        let k = t.scale();
        let (x, y) = t.apply(run.at);
        let system = self.window.text_system().clone();
        let lines: Vec<_> = run
            .spans
            .iter()
            .filter(|s| !s.text.is_empty())
            .map(|s| {
                let size = s.face.size_at(run.size) * k;
                let r = TextRun {
                    len: s.text.len(),
                    font: self.fonts.font(s.face),
                    ..TextRun::default()
                };
                (
                    s,
                    size,
                    system.layout_line(&s.text, px(size as f32), &[r], None),
                )
            })
            .collect();
        let width: f64 = lines.iter().map(|(_, _, l)| f64::from(l.width)).sum();
        let mut at = match run.anchor {
            Align::Start => x,
            Align::Middle => x - width / 2.0,
            Align::End => x - width,
        };
        let color = hsla(ink);
        for (s, size, line) in &lines {
            let base = y + shift(s.face, run.size) * k;
            for r in &line.runs {
                for g in &r.glyphs {
                    let o = point(px(at as f32) + g.position.x, px(base as f32) + g.position.y);
                    let font_size = px(*size as f32);
                    let _ = if g.is_emoji {
                        self.window.paint_emoji(o, r.font_id, g.id, font_size)
                    } else {
                        self.window
                            .paint_glyph(o, r.font_id, g.id, font_size, color)
                    };
                }
            }
            let w = f64::from(line.width);
            if s.face.contains(Face::UNDERLINE) {
                // GPUI keeps a font's metrics to itself: a tenth of the size
                // under the baseline, a fifteenth thick.
                let mut p = limu::scene::Path::new();
                let (top, thick) = (*size / 10.0, *size / 15.0);
                p.polygon(&[
                    (at, base + top),
                    (at + w, base + top),
                    (at + w, base + top + thick),
                    (at, base + top + thick),
                ]);
                let mut b = PathBuilder::fill().with_style(PathStyle::Fill(gpui::FillOptions {
                    fill_rule: gpui::FillRule::NonZero,
                }));
                trace(&mut b, &p, Affine::IDENTITY);
                if let Ok(p) = b.build() {
                    self.window.paint_path(p, color);
                }
            }
            at += w;
        }
    }

    /// Paints a run turned, or as a halo, as SVG text.
    fn svg_text(&mut self, run: &Run<'_>, ink: &Ink, halo: Option<f64>) {
        let t = self.at();
        let (x, y) = t.apply(run.at);
        let turn = t.angle();
        let text = svg::text_svg(self.fonts, run, t.scale(), halo);
        let h = text.half as f32;
        let bounds = Bounds::new(
            point(px(x as f32 - h), px(y as f32 - h)),
            size(px(2.0 * h), px(2.0 * h)),
        );
        // GPUI turns a sprite about the window's origin, in device pixels.
        let k = self.window.scale_factor();
        let c = point(
            gpui::ScaledPixels(x as f32 * k),
            gpui::ScaledPixels(y as f32 * k),
        );
        let m = TransformationMatrix::unit()
            .translate(c)
            .rotate(radians(turn as f32))
            .translate(point(
                gpui::ScaledPixels(-c.x.0),
                gpui::ScaledPixels(-c.y.0),
            ));
        let mut hasher = std::hash::DefaultHasher::new();
        text.svg.hash(&mut hasher);
        let key = SharedString::from(format!("limu-text-{:016x}", hasher.finish()));
        let _ = self.window.paint_svg(
            bounds,
            key,
            Some(text.svg.as_bytes()),
            m,
            hsla(ink),
            self.cx,
        );
    }
}

impl Canvas for GpuiCanvas<'_> {
    fn has_layers(&self) -> bool {
        false
    }

    fn begin_rotate(&mut self, degrees: f64, about: Point) {
        let t = self.at().then(Affine::rotate(degrees, about));
        self.transforms.push(t);
    }

    fn end_rotate(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn path(&mut self, path: &limu::scene::Path, fill: Option<&Ink>, pen: Option<&Pen<'_>>) {
        if let Some(ink) = fill {
            self.fill(path, ink);
        }
        if let Some(pen) = pen {
            self.stroke(path, &pen.ink, pen.width, pen.cap, pen.join, pen.dash);
        }
    }

    fn text(&mut self, run: &Run<'_>) {
        match &run.paint {
            TextPaint::Halo { ink, width } => self.svg_text(run, ink, Some(*width)),
            TextPaint::Fill(ink) if self.at().angle() != 0.0 => self.svg_text(run, ink, None),
            TextPaint::Fill(ink) => self.glyphs(run, ink),
        }
    }

    /// Icons are not supplied to the scene, so they are left out.
    fn icon(&mut self, _name: &str, _bounds: Rect, _ink: &Ink) {}

    /// Images are not supplied to the scene, so they are left out.
    fn image(&mut self, _src: &str, _bounds: Rect, _fit: bool, _opacity: f64) {}
}

/// An affine transform, x' = a x + c y + e and y' = b x + d y + f.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Affine {
    const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// A scene's units placed at a point, each k pixels.
    fn place(x: f64, y: f64, k: f64) -> Affine {
        Affine {
            a: k,
            d: k,
            e: x,
            f: y,
            ..Affine::IDENTITY
        }
    }

    /// A turn by degrees about a point, clockwise on the screen.
    fn rotate(degrees: f64, about: Point) -> Affine {
        let (sin, cos) = degrees.to_radians().sin_cos();
        Affine {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            e: about.x - cos * about.x + sin * about.y,
            f: about.y - sin * about.x - cos * about.y,
        }
    }

    /// This transform after another: the other is applied first.
    fn then(self, m: Affine) -> Affine {
        Affine {
            a: self.a * m.a + self.c * m.b,
            b: self.b * m.a + self.d * m.b,
            c: self.a * m.c + self.c * m.d,
            d: self.b * m.c + self.d * m.d,
            e: self.a * m.e + self.c * m.f + self.e,
            f: self.b * m.e + self.d * m.f + self.f,
        }
    }

    fn apply(self, p: Point) -> (f64, f64) {
        (
            self.a * p.x + self.c * p.y + self.e,
            self.b * p.x + self.d * p.y + self.f,
        )
    }

    /// How much the transform scales, the same both ways.
    fn scale(self) -> f64 {
        self.a.hypot(self.b)
    }

    /// How far the transform turns, in radians, clockwise on the screen.
    fn angle(self) -> f64 {
        self.b.atan2(self.a)
    }
}

/// How far a span's baseline is lowered from the run's: a fifth of the label's
/// size for a sub span, a third raised for a sup span.
fn shift(face: Face, size: f64) -> f64 {
    if face.contains(Face::SUB) {
        size / 5.0
    } else if face.contains(Face::SUP) {
        -size / 3.0
    } else {
        0.0
    }
}

/// Adds a scene's path, placed by a transform, to a path builder.
fn trace(b: &mut PathBuilder, path: &limu::scene::Path, t: Affine) {
    let at = |p: Point| {
        let (x, y) = t.apply(p);
        point(px(x as f32), px(y as f32))
    };
    for step in path.steps() {
        match step {
            Step::MoveTo(p) => b.move_to(at(p)),
            Step::LineTo(p) => b.line_to(at(p)),
            Step::QuadTo(c, p) => b.curve_to(at(p), at(c)),
            Step::CubeTo(c1, c2, p) => b.cubic_bezier_to(at(p), at(c1), at(c2)),
            Step::Close => b.close(),
        }
    }
}

fn hsla(ink: &Ink) -> Hsla {
    let c = ink.color;
    let k = |v: u8| f32::from(v) / 255.0;
    Rgba {
        r: k(c.r),
        g: k(c.g),
        b: k(c.b),
        a: k(c.a),
    }
    .into()
}

/// A dash as GPUI strokes it, none for solid: one that SVG draws solid (a
/// negative length, or none but zeros) is solid.
fn dash_of(dash: &[f64]) -> &[f64] {
    if dash.iter().any(|&d| d.is_nan() || d < 0.0) || dash.iter().all(|&d| d == 0.0) {
        return &[];
    }
    dash
}
