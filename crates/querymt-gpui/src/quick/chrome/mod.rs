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

use super::hud::{DARK_MATERIAL_TINT, TINT};
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
        }
    }

    /// A material the panel itself keeps dark (macOS) is drawn behind the
    /// window by a view of ours. `rimmed`: it draws its own edge.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn dark_material(rimmed: bool) -> Self {
        Self {
            glass: true,
            radius: px(RADIUS),
            tint: DARK_MATERIAL_TINT,
            rimmed,
            native_motion: true,
        }
    }

    /// The root's colours for `p`, the HUD palette (design D15): over a
    /// material a black tint of [`Self::tint`] edged by a light hairline
    /// (none where the material has its own edge); without a material the
    /// palette's own, which are opaque.
    pub(crate) fn surfaces(&self, p: &Palette) -> Surfaces {
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
pub(crate) fn install(window: &mut Window, animate: bool) -> Chrome {
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

/// Give the window `size`. On macOS the **bottom edge stays fixed**, so the
/// composer does not move under the cursor while the panel grows; elsewhere
/// GPUI has no reposition call and the top left stays (design D13).
pub(crate) fn resize(window: &mut Window, size: Size<Pixels>, cx: &App) {
    #[cfg(target_os = "macos")]
    if macos::set_height(window, size, cx) {
        return;
    }
    let _ = cx;
    window.resize(size);
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

    // quick-panel / Uses its desktop's material: where the panel keeps the
    // material dark itself, nothing of ours covers it.
    #[test]
    fn a_dark_material_gets_no_fill_and_a_rimmed_one_no_hairline() {
        let p = hud_palette(&Palette::light(), true);
        let glass = Chrome::dark_material(true).surfaces(&p);
        assert!(glass.bg.a < Chrome::glass().surfaces(&p).bg.a);
        assert_eq!(glass.border.a, 0., "Liquid Glass draws its own edge");
        let effect = Chrome::dark_material(false).surfaces(&p);
        assert!(effect.border.a > 0., "a visual-effect view has none");
        assert!(Chrome::dark_material(true).native_motion);
        assert!(!Chrome::glass().native_motion && !Chrome::solid().native_motion);
    }

    #[test]
    fn the_panel_is_rounder_than_a_card() {
        assert_eq!(super::RADIUS, 20.);
    }
}
