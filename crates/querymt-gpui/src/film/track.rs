//! Per-window capture (design D5): canvas placement, the rounded alpha mask,
//! compositing and the encoder processes.
//!
//! Capture is opaque (`MetalHeadlessRenderer` clears to alpha 1), so the
//! quick panel's corners come out black. The harness masks them itself.

use std::io::Write as _;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// One captured frame: RGBA8 pixels, row by row.
pub(super) struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    /// A fully transparent frame.
    pub(super) fn transparent(width: u32, height: u32) -> Self {
        Frame {
            width,
            height,
            rgba: vec![0; width as usize * height as usize * 4],
        }
    }
}

/// Distance from `(x, y)` to a rounded rectangle of `w` x `h` with corner
/// radius `r`, centred on the origin of its own box: negative inside.
fn rounded_distance(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy) = (x - w / 2.0, y - h / 2.0);
    let (qx, qy) = (cx.abs() - (w / 2.0 - r), cy.abs() - (h / 2.0 - r));
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Coverage (0..=255) of the pixel `(x, y)` of a `w` x `h` rounded rectangle
/// with corner radius `radius` (device pixels). The edge is antialiased over
/// one device pixel.
pub(super) fn mask_alpha(x: u32, y: u32, w: u32, h: u32, radius: f32) -> u8 {
    let d = rounded_distance(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
    ((0.5 - d).clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Multiply the frame's alpha by the rounded-rectangle mask.
pub(super) fn apply_mask(frame: &mut Frame, radius: f32) {
    let (w, h) = (frame.width, frame.height);
    for y in 0..h {
        for x in 0..w {
            let mask = u32::from(mask_alpha(x, y, w, h, radius));
            let a = &mut frame.rgba[(y as usize * w as usize + x as usize) * 4 + 3];
            *a = ((u32::from(*a) * mask + 127) / 255) as u8;
        }
    }
}

/// `image` bottom-centred on a transparent canvas; returns the canvas and
/// the image's top-left corner on it.
pub(super) fn place_bottom_center(
    image: &Frame,
    canvas_w: u32,
    canvas_h: u32,
) -> (Frame, (u32, u32)) {
    assert!(
        image.width <= canvas_w && image.height <= canvas_h,
        "the panel ({}x{}) does not fit its canvas ({canvas_w}x{canvas_h})",
        image.width,
        image.height
    );
    let mut canvas = Frame::transparent(canvas_w, canvas_h);
    let (ox, oy) = ((canvas_w - image.width) / 2, canvas_h - image.height);
    for row in 0..image.height as usize {
        let from = row * image.width as usize * 4;
        let to = ((oy as usize + row) * canvas_w as usize + ox as usize) * 4;
        let len = image.width as usize * 4;
        canvas.rgba[to..to + len].copy_from_slice(&image.rgba[from..from + len]);
    }
    (canvas, (ox, oy))
}

/// Soft shadow under the panel: how dark it is at its darkest, how far it
/// reaches and how far it is pushed down (device pixels per scale unit).
const SHADOW_ALPHA: f32 = 0.38;
const SHADOW_BLUR: f32 = 28.0;
const SHADOW_DROP: f32 = 10.0;

/// Blend `src` over `dst` (straight alpha, `dst` opaque) at `(x, y)`.
fn blend(dst: &mut [u8], src: [u8; 4], coverage: f32) {
    let a = f32::from(src[3]) / 255.0 * coverage;
    for c in 0..3 {
        dst[c] = (f32::from(src[c]) * a + f32::from(dst[c]) * (1.0 - a)).round() as u8;
    }
}

/// Draw the masked `panel` over `main` with its top-left corner at `(x, y)`
/// (may be partly outside), with a soft shadow and a corner radius of
/// `radius` for the shadow's shape. `scale` is the device scale.
pub(super) fn composite(main: &mut Frame, panel: &Frame, x: i64, y: i64, radius: f32, scale: f32) {
    let (pw, ph) = (panel.width as f32, panel.height as f32);
    let blur = SHADOW_BLUR * scale;
    let drop = SHADOW_DROP * scale;
    let reach = (blur + drop).ceil() as i64;
    let (mw, mh) = (i64::from(main.width), i64::from(main.height));
    for my in (y - reach).max(0)..(y + panel.height as i64 + reach).min(mh) {
        for mx in (x - reach).max(0)..(x + panel.width as i64 + reach).min(mw) {
            let (px, py) = ((mx - x) as f32 + 0.5, (my - y) as f32 + 0.5);
            let at = (my as usize * main.width as usize + mx as usize) * 4;
            // The shadow: the panel's outline, pushed down, fading out.
            let d = rounded_distance(px, py - drop, pw, ph, radius);
            let falloff = (1.0 - (d / blur).clamp(0.0, 1.0)).powi(2);
            if falloff > 0.0 {
                blend(
                    &mut main.rgba[at..at + 4],
                    [0, 0, 0, 255],
                    SHADOW_ALPHA * falloff,
                );
            }
            // The panel itself.
            if (0.0..pw).contains(&px) && (0.0..ph).contains(&py) {
                let p = (py as usize * panel.width as usize + px as usize) * 4;
                let src = [
                    panel.rgba[p],
                    panel.rgba[p + 1],
                    panel.rgba[p + 2],
                    panel.rgba[p + 3],
                ];
                blend(&mut main.rgba[at..at + 4], src, 1.0);
            }
        }
    }
}

/// How the panel track is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelCodec {
    /// VP9 with alpha, in WebM.
    Vp9,
    /// ProRes 4444 with alpha, for editors (`$QMT_FILM_PANEL_CODEC=prores`).
    ProRes,
}

impl PanelCodec {
    pub(super) fn from_env(value: Option<&str>) -> Self {
        match value {
            Some("prores") => PanelCodec::ProRes,
            _ => PanelCodec::Vp9,
        }
    }

    pub(super) fn file_name(self) -> &'static str {
        match self {
            PanelCodec::Vp9 => "panel.webm",
            PanelCodec::ProRes => "panel.mov",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            PanelCodec::Vp9 => &[
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                "yuva420p",
                "-b:v",
                "0",
                "-crf",
                "24",
                "-auto-alt-ref",
                "0",
                "-metadata:s:v:0",
                "alpha_mode=1",
            ],
            PanelCodec::ProRes => &[
                "-c:v",
                "prores_ks",
                "-profile:v",
                "4444",
                "-pix_fmt",
                "yuva444p10le",
            ],
        }
    }
}

/// An `ffmpeg` process fed raw RGBA frames over stdin.
pub(super) struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    width: u32,
    height: u32,
}

impl Encoder {
    fn spawn(path: &Path, width: u32, height: u32, fps: u32, codec_args: &[&str]) -> Self {
        let mut child = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
            ])
            .args(["-s", &format!("{width}x{height}"), "-r", &fps.to_string()])
            .args(["-i", "-"])
            .args(codec_args)
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("ffmpeg must be on PATH to film demo clips");
        let stdin = child.stdin.take();
        Encoder {
            child,
            stdin,
            width,
            height,
        }
    }

    /// H.264 for the opaque main track.
    pub(super) fn main(path: &Path, width: u32, height: u32, fps: u32) -> Self {
        Self::spawn(
            path,
            width,
            height,
            fps,
            &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "18"],
        )
    }

    /// The panel's alpha track.
    pub(super) fn panel(path: &Path, width: u32, height: u32, fps: u32, codec: PanelCodec) -> Self {
        Self::spawn(path, width, height, fps, codec.args())
    }

    pub(super) fn write(&mut self, frame: &Frame) {
        assert_eq!(
            (frame.width, frame.height),
            (self.width, self.height),
            "every frame of a track has the same size"
        );
        self.stdin
            .as_mut()
            .expect("the encoder is open")
            .write_all(&frame.rgba)
            .expect("pipe frame");
    }

    /// Close the input and wait for the file; returns how long that took.
    pub(super) fn finish(mut self) -> Duration {
        drop(self.stdin.take());
        let started = Instant::now();
        let status = self.child.wait().expect("wait for ffmpeg");
        assert!(status.success(), "ffmpeg failed: {status}");
        started.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mask_is_clear_in_the_corner_solid_in_the_middle_and_soft_on_the_edge() {
        let (w, h, radius) = (200, 120, 40.0);
        assert_eq!(mask_alpha(0, 0, w, h, radius), 0);
        assert_eq!(mask_alpha(w - 1, h - 1, w, h, radius), 0);
        assert_eq!(mask_alpha(100, 60, w, h, radius), 255);
        // Straight edges are solid right up to the border.
        assert_eq!(mask_alpha(100, 0, w, h, radius), 255);
        assert_eq!(mask_alpha(0, 60, w, h, radius), 255);
        // On the arc the coverage is in between: along the diagonal of the
        // corner, some pixel is neither clear nor solid.
        let edge: Vec<u8> = (0..40).map(|i| mask_alpha(i, i, w, h, radius)).collect();
        assert!(edge.iter().any(|a| *a > 0 && *a < 255), "{edge:?}");
        // And it only grows towards the inside.
        assert!(edge.windows(2).all(|p| p[0] <= p[1]), "{edge:?}");
    }

    #[test]
    fn apply_mask_makes_the_corners_transparent_and_keeps_the_colour() {
        let mut frame = Frame {
            width: 64,
            height: 64,
            rgba: [10, 20, 30, 255].repeat(64 * 64),
        };
        apply_mask(&mut frame, 16.0);
        assert_eq!(frame.rgba[3], 0, "alpha at 0,0");
        let mid = (32 * 64 + 32) * 4;
        assert_eq!(&frame.rgba[mid..mid + 4], &[10, 20, 30, 255]);
        assert_eq!(&frame.rgba[0..3], &[10, 20, 30]);
    }

    #[test]
    fn the_panel_sits_bottom_centre_on_a_transparent_canvas() {
        let image = Frame {
            width: 4,
            height: 2,
            rgba: [9, 9, 9, 255].repeat(8),
        };
        let (canvas, (x, y)) = place_bottom_center(&image, 10, 6);
        assert_eq!((x, y), (3, 4));
        let at = |x: usize, y: usize| &canvas.rgba[(y * 10 + x) * 4..(y * 10 + x) * 4 + 4];
        assert_eq!(at(3, 4), &[9, 9, 9, 255]);
        assert_eq!(at(6, 5), &[9, 9, 9, 255]);
        assert_eq!(at(2, 5), &[0, 0, 0, 0]);
        assert_eq!(at(5, 3), &[0, 0, 0, 0]);
    }

    #[test]
    fn composite_draws_the_panel_and_darkens_the_area_below_it() {
        let mut main = Frame {
            width: 120,
            height: 120,
            rgba: [200, 200, 200, 255].repeat(120 * 120),
        };
        let mut panel = Frame {
            width: 40,
            height: 20,
            rgba: [10, 20, 30, 255].repeat(40 * 20),
        };
        apply_mask(&mut panel, 8.0);
        composite(&mut main, &panel, 40, 50, 8.0, 1.0);
        let at = |x: usize, y: usize| main.rgba[(y * 120 + x) * 4..(y * 120 + x) * 4 + 3].to_vec();
        // Inside the panel.
        assert_eq!(at(60, 60), vec![10, 20, 30]);
        // The corner is rounded: the background (and its shadow) shows.
        assert!(at(40, 50)[0] > 100, "{:?}", at(40, 50));
        // Just below the panel the shadow darkens the background.
        assert!(at(60, 72)[0] < 200, "{:?}", at(60, 72));
        // Far away nothing changed.
        assert_eq!(at(2, 2), vec![200, 200, 200]);
    }

    #[test]
    fn the_panel_codec_follows_the_environment() {
        assert_eq!(PanelCodec::from_env(None), PanelCodec::Vp9);
        assert_eq!(PanelCodec::from_env(Some("prores")), PanelCodec::ProRes);
        assert_eq!(PanelCodec::from_env(Some("x")), PanelCodec::Vp9);
        assert_eq!(PanelCodec::Vp9.file_name(), "panel.webm");
        assert_eq!(PanelCodec::ProRes.file_name(), "panel.mov");
    }
}
