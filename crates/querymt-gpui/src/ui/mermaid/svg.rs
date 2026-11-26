// Ported from limu-gpui 0.2.1 (MIT, Copyright (c) Firede); see paint.rs.
//! Text GPUI can't draw itself, written as SVG for GPUI's SVG renderer to draw.
//!
//! GPUI draws a glyph only upright and only filled. Its SVG renderer draws text in the system's
//! fonts and the fonts the app bundles, and `paint_svg` turns what it draws by a matrix, as
//! GPUI's icons are turned. Text that is turned or drawn as a halo is written as an SVG of
//! its own, the run's anchor at the middle of a square, so that turning the square about its
//! middle turns the text about its anchor.

use std::fmt::Write;

use limu::scene::text::Face;
use limu::scene::{Align, Run};

use super::paint::Fonts;

/// A run of text as an SVG document, filled or stroked in black, which GPUI draws as a mask
/// in the run's color.
pub(crate) struct TextSvg {
    /// The document.
    pub(crate) svg: String,
    /// Half the side of the square it draws in, in pixels. The run is anchored at its middle.
    pub(crate) half: f64,
}

/// A run as an SVG document, in pixels of a scale, its text stroked a width wide where it is
/// a halo, filled otherwise.
pub(crate) fn text_svg(fonts: &Fonts, run: &Run<'_>, scale: f64, halo: Option<f64>) -> TextSvg {
    let width: f64 = run.spans.iter().map(|s| s.width).sum();
    // Room for the run whichever way it is anchored and turned, and for its halo.
    let half = ((width + 2.0 * run.size + halo.unwrap_or(0.0)) * scale).ceil();
    let side = 2.0 * half;
    let anchor = match run.anchor {
        Align::Start => "start",
        Align::Middle => "middle",
        Align::End => "end",
    };
    let paint = match halo {
        Some(w) => format!(
            r##"fill="none" stroke="#000" stroke-width="{}" stroke-linejoin="round""##,
            num(w * scale)
        ),
        None => r##"fill="#000""##.to_string(),
    };
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{s}" height="{s}" viewBox="0 0 {s} {s}"><text xml:space="preserve" x="{h}" y="{h}" text-anchor="{anchor}" font-family="{family}" font-size="{size}" {paint}>"#,
        s = num(side),
        h = num(half),
        family = escape(&fonts.sans),
        size = num(run.size * scale),
    );
    for span in run.spans.iter().filter(|s| !s.text.is_empty()) {
        svg.push_str("<tspan");
        let f = span.face;
        if f.contains(Face::MONO) {
            let _ = write!(svg, r#" font-family="{}""#, escape(&fonts.mono));
        }
        if f.contains(Face::BOLD) {
            svg.push_str(r#" font-weight="bold""#);
        }
        if f.contains(Face::ITALIC) {
            svg.push_str(r#" font-style="italic""#);
        }
        if f.contains(Face::UNDERLINE) {
            svg.push_str(r#" text-decoration="underline""#);
        }
        // Lowered a fifth or raised a third of the label's size, as the other backends set
        // them.
        let shift = if f.contains(Face::SUB) {
            Some(-run.size / 5.0)
        } else if f.contains(Face::SUP) {
            Some(run.size / 3.0)
        } else {
            None
        };
        if let Some(shift) = shift {
            let _ = write!(
                svg,
                r#" font-size="{}" baseline-shift="{}""#,
                num(f.size_at(run.size) * scale),
                num(shift * scale)
            );
        }
        let _ = write!(svg, ">{}</tspan>", escape(&span.text));
    }
    svg.push_str("</text></svg>");
    TextSvg { svg, half }
}

/// A number as it is written in SVG and in the records of the tests, to two places.
fn num(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

/// Text escaped for XML.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}
