//! The panel's chrome: the translucent material its desktop offers, and the
//! one flag the panel's render code reads (design D11).
//!
//! Each desktop gets its own look, not one faked everywhere: AppKit's
//! visual-effect (or glass) view on macOS, Mica or blur on Windows, compositor
//! blur where Wayland offers it, and an opaque surface otherwise.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub(super) mod macos;
#[cfg(target_os = "macos")]
mod rim;
#[cfg(target_os = "windows")]
mod windows;

use gpui_kit::*;

use super::hud::{
    LIQUID_GLASS_TINT, LIQUID_GLASS_WHITE_THROUGH, TINT, VFX_TINT, VFX_WHITE_THROUGH,
};
use crate::theme::Palette;

/// Corner radius of the panel's rounded root.
pub(crate) const RADIUS: f32 = 20.;

/// What the panel's surfaces are painted on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Chrome {
    /// A translucent material sits behind the window, so surfaces are
    /// see-through. Without it they are opaque.
    pub(crate) glass: bool,
    pub(crate) radius: Pixels,
    /// How much black the panel lays over the material (design D11): little
    /// or none where the panel makes the material dark itself, enough for
    /// text to stay legible where the desktop picks it.
    pub(crate) tint: f32,
    /// The material draws its own edge (Liquid Glass), so the panel paints no
    /// hairline over it.
    pub(crate) rimmed: bool,
    /// The material is a view of ours that the opening animation fades and
    /// scales together with the window (design D14), so the panel's root does
    /// not fade on its own.
    pub(crate) native_motion: bool,
    /// The most of a white backdrop the material lets through, 0 to 1 (an
    /// opaque surface 0). The legibility contract is held against it
    /// (design D11).
    pub(crate) white_through: f32,
    /// The window's root frames the panel ([`frame`]): the frame's hairline
    /// is the panel's, so the panel paints none of its own.
    pub(crate) framed: bool,
}

/// The colours of the panel's root, picked by [`Chrome`]. Everything inside
/// it is painted from the palette ([`super::hud::hud_palette`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Surfaces {
    /// The panel's root.
    pub(crate) bg: Hsla,
    /// The hairline around it.
    pub(crate) border: Hsla,
}

impl Chrome {
    /// No material: the palette's opaque colours.
    pub(crate) fn solid() -> Self {
        Self {
            glass: false,
            radius: px(RADIUS),
            tint: 1.,
            rimmed: false,
            native_motion: false,
            white_through: 0.,
            framed: false,
        }
    }

    /// A material the desktop picks (Mica, compositor blur) is drawn behind
    /// the window. It can be light, so the panel tints it dark.
    #[cfg_attr(target_os = "macos", allow(dead_code))] // Windows and Linux; tests everywhere
    pub(crate) fn glass() -> Self {
        Self {
            glass: true,
            radius: px(RADIUS),
            tint: TINT,
            rimmed: false,
            native_motion: false,
            // Mica and compositor blur are the system's to choose, and a white
            // window behind them comes through whole.
            white_through: 1.,
            framed: false,
        }
    }

    /// A material set to dark aqua (macOS) is drawn behind the window by a
    /// view of ours. `rimmed`: it is Liquid Glass, which draws its own edge;
    /// else the visual-effect material.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn dark_material(rimmed: bool) -> Self {
        let (tint, white_through) = if rimmed {
            (LIQUID_GLASS_TINT, LIQUID_GLASS_WHITE_THROUGH)
        } else {
            (VFX_TINT, VFX_WHITE_THROUGH)
        };
        Self {
            glass: true,
            radius: px(RADIUS),
            tint,
            rimmed,
            native_motion: true,
            white_through,
            framed: false,
        }
    }

    /// Where the window's root frames the panel ([`frame`]), the panel takes
    /// the frame's corners, so no gap shows between the frame's rounded
    /// hairline and the panel's surface, and leaves the hairline to it.
    pub(crate) fn within_frame(mut self, window: &Window, cx: &App) -> Self {
        if frame(window) != Edges::default() {
            self.radius = gpui_kit::component::window_corner_radii(window, cx).top_left;
            self.framed = true;
        }
        self
    }

    /// The root's colours for `p`, the HUD palette (design D15): over a
    /// material a black tint of [`Self::tint`] edged by a light hairline
    /// (none where the material has its own edge); without a material the
    /// palette's own, which are opaque.
    pub(crate) fn surfaces(&self, p: &Palette) -> Surfaces {
        let surfaces = self.material_surfaces(p);
        if self.framed {
            Surfaces {
                border: gpui_kit::transparent_black(),
                ..surfaces
            }
        } else {
            surfaces
        }
    }

    fn material_surfaces(&self, p: &Palette) -> Surfaces {
        if self.glass {
            Surfaces {
                bg: gpui_kit::black().opacity(self.tint),
                border: gpui_kit::white().opacity(if self.rimmed { 0. } else { 0.14 }),
            }
        } else {
            Surfaces {
                bg: p.bg,
                border: p.border,
            }
        }
    }
}

/// Install the desktop's material behind `window` and report what it got. On
/// macOS the material is set dark once, whatever the app theme or the desktop
/// is set to (the panel is a HUD, D15). `animate`: the opening animation will
/// run, so the material starts invisible and [`open_step`] brings it in.
///
/// When the system asks to reduce transparency, no material is installed at
/// all and the panel is the opaque one, as on a desktop without a material
/// (design D11).
///
/// On macOS the window has no system shadow either way: it would follow the
/// window's outline, not the panel's (see `macos::drop_window_shadow`).
pub(crate) fn install(window: &mut Window, animate: bool) -> Chrome {
    #[cfg(target_os = "macos")]
    macos::drop_window_shadow(window);
    chrome_for(reduce_transparency(), || {
        #[cfg(target_os = "macos")]
        {
            macos::install(window, animate)
        }
        #[cfg(target_os = "windows")]
        {
            let _ = animate;
            windows::install(window)
        }
        #[cfg(target_os = "linux")]
        {
            let _ = animate;
            linux::install(window)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            let _ = (window, animate);
            Chrome::solid()
        }
    })
}

/// The chrome for a system that does, or does not, ask to reduce
/// transparency; `material` installs the desktop's and runs only when it is
/// not asked.
fn chrome_for(reduce_transparency: bool, material: impl FnOnce() -> Chrome) -> Chrome {
    if reduce_transparency {
        Chrome::solid()
    } else {
        material()
    }
}

/// The system asks apps for less transparency (macOS Reduce transparency,
/// Windows Transparency effects off). Not read on other systems.
fn reduce_transparency() -> bool {
    #[cfg(target_os = "macos")]
    return macos::reduce_transparency();
    #[cfg(target_os = "windows")]
    return windows::reduce_transparency();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    false
}

/// A renderer of the light along the edge outside GPUI, where there is one
/// (design D14): Core Animation on macOS, over the panel's own material
/// ([`Chrome::native_motion`]). `origin` is the edge's clock. `None`
/// elsewhere, or when it cannot be made: the panel then draws the ring.
pub(crate) fn edge_light(
    chrome: &Chrome,
    window: &Window,
    origin: std::time::Instant,
) -> Option<Box<dyn super::edge::EdgeLight>> {
    #[cfg(target_os = "macos")]
    if chrome.native_motion {
        return match rim::CoreAnimationRim::install(window, origin) {
            Ok(rim) => Some(Box::new(rim)),
            Err(err) => {
                tracing::warn!(%err, "no edge light; drawing the ring");
                None
            }
        };
    }
    let _ = (chrome, window, origin);
    None
}

/// One step of the opening animation for a material of ours
/// ([`Chrome::native_motion`]): the window at `opacity`, the material at
/// `scale` of the window around its centre. A no-op elsewhere.
pub(crate) fn open_step(window: &Window, opacity: f32, scale: f32) {
    #[cfg(target_os = "macos")]
    macos::open_step(window, opacity, scale);
    #[cfg(not(target_os = "macos"))]
    let _ = (window, opacity, scale);
}

/// The system saves power and asks apps to hold back on motion: Low Power
/// Mode on macOS (design D14). Always false elsewhere.
pub(crate) fn low_power() -> bool {
    #[cfg(target_os = "macos")]
    return macos::low_power();
    #[cfg(not(target_os = "macos"))]
    false
}

/// What the window's root draws around the panel, inside the window's size:
/// on Linux, gpui-component frames a client-decorated window (GNOME, and any
/// layer-shell surface) with a shadow margin and a hairline. Zero elsewhere.
pub(crate) fn frame(window: &Window) -> Edges<Pixels> {
    #[cfg(target_os = "linux")]
    {
        linux::frame(window)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window;
        Edges::default()
    }
}

/// The window size that leaves `size` for the panel inside `frame`.
fn framed_size(size: Size<Pixels>, frame: &Edges<Pixels>) -> Size<Pixels> {
    Size {
        width: size.width + frame.left + frame.right,
        height: size.height + frame.top + frame.bottom,
    }
}

/// Give the panel `size`; the window is larger by the [`frame`] around it.
/// On macOS the **bottom edge stays fixed**, so the composer does not move
/// under the cursor while the panel grows; elsewhere GPUI has no reposition
/// call and the top left stays (design D13).
pub(crate) fn resize(window: &mut Window, size: Size<Pixels>, cx: &App) {
    #[cfg(target_os = "macos")]
    if macos::set_height(window, size, cx) {
        return;
    }
    let _ = cx;
    let size = framed_size(size, &frame(window));
    window.resize(size);
}

/// The window takes a size asked of it only after the frame that asked for it
/// has been drawn: GPUI's Linux backends apply a resize from a task, so a
/// frame can still be drawn, and shown, in the old size. The panel then holds
/// what is on screen until the window has the new size (design D13). AppKit
/// and Windows resize at once.
pub(crate) const RESIZE_LAGS: bool = cfg!(target_os = "linux");

/// Keep the bottom edge where it was while the panel's height goes from
/// `from` to `to`, as macOS does in [`resize`] itself (design D13). Call it
/// while drawing the first frame of height `to`: on Wayland a toplevel cannot
/// place itself, and the move (`wl_surface.offset`) is applied by the next
/// commit, which must be the one that brings the new size. A move sent with
/// the resize would be applied to a frame of the old size, which would jump
/// by the difference. A no-op elsewhere.
pub(crate) fn keep_bottom(window: &Window, from: f32, to: f32) {
    #[cfg(target_os = "linux")]
    linux::keep_bottom(window, from, to);
    #[cfg(not(target_os = "linux"))]
    let _ = (window, from, to);
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use super::Chrome;
    use crate::quick::hud::{TINT, hud_palette};
    use crate::theme::Palette;

    #[test]
    fn chrome_without_a_material_is_opaque() {
        for user in [Palette::light(), Palette::dark()] {
            let p = hud_palette(&user, false);
            let s = Chrome::solid().surfaces(&p);
            assert_eq!(s.bg, p.bg);
            assert_eq!(s.border, p.border);
            assert_eq!(s.bg.a, 1.0, "solid chrome leaves no see-through area");
        }
    }

    #[test]
    fn glass_and_solid_chrome_pick_different_surfaces() {
        for user in [Palette::light(), Palette::dark()] {
            let p = hud_palette(&user, true);
            let glass = Chrome::glass().surfaces(&p);
            let solid = Chrome::solid().surfaces(&hud_palette(&user, false));
            assert_ne!(glass, solid);
            assert_eq!(
                glass.bg.a, TINT,
                "the root is a tint the material shows through"
            );
            assert!(glass.border.a > 0.0 && glass.border.a < 1.0);
        }
    }

    #[test]
    fn glass_hairline_is_light_on_the_dark_tint() {
        let s = Chrome::glass().surfaces(&hud_palette(&Palette::light(), true));
        assert!(s.border.l > 0.5);
        assert!(s.bg.l < 0.1);
    }

    // quick-panel / Bright window behind the panel: a dark aqua material still
    // comes out light grey over a white window (task 7.19), so the panel darkens
    // it; `hud::text_passes_over_a_white_backdrop` sets how much.
    #[test]
    fn a_macos_material_is_darkened_and_a_rimmed_one_has_no_hairline() {
        let p = hud_palette(&Palette::light(), true);
        let glass = Chrome::dark_material(true).surfaces(&p);
        let effect = Chrome::dark_material(false).surfaces(&p);
        for bg in [glass.bg, effect.bg] {
            assert!(bg.a >= 0.6, "a black tint of its own: {bg:?}");
            assert!(bg.l < 0.01);
        }
        assert_eq!(glass.border.a, 0., "Liquid Glass draws its own edge");
        assert!(effect.border.a > 0., "a visual-effect view has none");
        assert!(Chrome::dark_material(true).native_motion);
        assert!(!Chrome::glass().native_motion && !Chrome::solid().native_motion);
    }

    // quick-panel / Reduce transparency
    #[test]
    fn reduce_transparency_gives_the_opaque_chrome_and_installs_no_material() {
        let mut installed = false;
        let chrome = super::chrome_for(true, || {
            installed = true;
            Chrome::dark_material(true)
        });
        assert_eq!(chrome, Chrome::solid());
        assert!(!chrome.glass && !chrome.native_motion);
        assert!(!installed, "no material view is added");
        // Without the setting the desktop's material is used.
        let chrome = super::chrome_for(false, Chrome::glass);
        assert!(chrome.glass);
    }

    #[test]
    fn the_panel_is_rounder_than_a_card() {
        assert_eq!(super::RADIUS, 20.);
    }

    #[test]
    fn a_framed_panel_leaves_the_hairline_to_the_frame() {
        let p = hud_palette(&Palette::dark(), false);
        let framed = Chrome {
            framed: true,
            ..Chrome::solid()
        };
        let (own, inside) = (Chrome::solid().surfaces(&p), framed.surfaces(&p));
        assert_eq!(inside.bg, own.bg);
        assert_eq!(inside.border.a, 0.);
    }

    #[test]
    fn the_window_is_larger_than_the_panel_by_its_frame() {
        use gpui_kit::{Edges, px, size};
        let frame = Edges {
            top: px(21.),
            right: px(21.),
            bottom: px(21.),
            left: px(21.),
        };
        let panel = size(px(super::super::panel::WIDTH), px(64.));
        assert_eq!(
            super::framed_size(panel, &frame),
            size(px(super::super::panel::WIDTH + 42.), px(106.))
        );
        assert_eq!(super::framed_size(panel, &Edges::default()), panel);
    }
}
