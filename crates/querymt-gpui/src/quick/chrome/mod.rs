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
    LIQUID_GLASS_GREY_OVER_BLACK, LIQUID_GLASS_LIGHT_TINT, LIQUID_GLASS_TINT,
    LIQUID_GLASS_WHITE_THROUGH, TINT, VFX_GREY_OVER_BLACK, VFX_LIGHT_TINT, VFX_TINT,
    VFX_WHITE_THROUGH,
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
    /// where the panel sets the material's appearance itself, enough for text
    /// to stay legible where the desktop picks it.
    pub(crate) tint: f32,
    /// The same for a light panel, which lays white over it.
    pub(crate) light_tint: f32,
    /// The material draws its own edge (Liquid Glass), so the panel paints no
    /// hairline over it.
    pub(crate) rimmed: bool,
    /// The material is a view of ours that the opening animation fades and
    /// scales together with the window (design D14), so the panel's root does
    /// not fade on its own.
    pub(crate) native_motion: bool,
    /// The most of a white backdrop the material lets through, 0 to 1 (an
    /// opaque surface 0). The legibility contract is held against it for a
    /// dark panel (design D11).
    pub(crate) white_through: f32,
    /// The lightest grey the material shows over a black backdrop, 0 to 1:
    /// what the contract is held against for a light panel.
    pub(crate) grey_over_black: f32,
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
            light_tint: 1.,
            rimmed: false,
            native_motion: false,
            white_through: 0.,
            grey_over_black: 0.,
        }
    }

    /// A material the desktop picks (Mica, compositor blur) is drawn behind
    /// the window. It can be light or dark, so the panel tints it towards its
    /// own appearance.
    #[cfg_attr(target_os = "macos", allow(dead_code))] // Windows and Linux; tests everywhere
    pub(crate) fn glass() -> Self {
        Self {
            glass: true,
            radius: px(RADIUS),
            tint: TINT,
            light_tint: TINT,
            rimmed: false,
            native_motion: false,
            // Mica and compositor blur are the system's to choose, and a white
            // window behind them comes through whole.
            white_through: 1.,
            grey_over_black: 0.,
        }
    }

    /// A material set to the app's appearance (macOS) is drawn behind the
    /// window by a view of ours. `rimmed`: it is Liquid Glass, which draws its
    /// own edge; else the visual-effect material.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn material(rimmed: bool) -> Self {
        let (tint, light_tint, white_through, grey_over_black) = if rimmed {
            (
                LIQUID_GLASS_TINT,
                LIQUID_GLASS_LIGHT_TINT,
                LIQUID_GLASS_WHITE_THROUGH,
                LIQUID_GLASS_GREY_OVER_BLACK,
            )
        } else {
            (
                VFX_TINT,
                VFX_LIGHT_TINT,
                VFX_WHITE_THROUGH,
                VFX_GREY_OVER_BLACK,
            )
        };
        Self {
            glass: true,
            radius: px(RADIUS),
            tint,
            light_tint,
            rimmed,
            native_motion: true,
            white_through,
            grey_over_black,
        }
    }

    /// The grey level (0 black, 1 white) of the worst backdrop the material
    /// shows for a panel of the given appearance: a white window behind a dark
    /// panel (`white_through`), a black one behind a light panel
    /// (`grey_over_black`).
    #[cfg(test)]
    pub(crate) fn worst_backdrop(&self, dark: bool) -> f32 {
        if dark {
            self.white_through
        } else {
            self.grey_over_black
        }
    }

    /// The root's colours for `p`, the HUD palette (design D15): over a
    /// material a tint of [`Self::tint`] (black, for a dark panel) or
    /// [`Self::light_tint`] (white, for a light one) edged by a hairline of the opposite colour (none where the
    /// material has its own edge); without a material the palette's own,
    /// which are opaque.
    pub(crate) fn surfaces(&self, p: &Palette) -> Surfaces {
        if self.glass {
            let (tint, strength, edge) = if p.dark {
                (
                    gpui_kit::black(),
                    self.tint,
                    gpui_kit::white().opacity(0.14),
                )
            } else {
                (
                    gpui_kit::white(),
                    self.light_tint,
                    gpui_kit::black().opacity(0.12),
                )
            };
            Surfaces {
                bg: tint.opacity(strength),
                border: if self.rimmed { edge.opacity(0.) } else { edge },
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
/// macOS the material is set to the app's appearance (`dark`), not the
/// desktop's, and follows it with [`follow_theme`] (D11, D15). `animate`: the
/// opening animation will run, so the material starts invisible and
/// [`open_step`] brings it in.
///
/// When the system asks to reduce transparency, no material is installed at
/// all and the panel is the opaque one, as on a desktop without a material
/// (design D11).
///
/// On macOS the window has no system shadow either way: it would follow the
/// window's outline, not the panel's (see `macos::drop_window_shadow`).
pub(crate) fn install(window: &mut Window, animate: bool, dark: bool) -> Chrome {
    #[cfg(target_os = "macos")]
    macos::drop_window_shadow(window);
    chrome_for(reduce_transparency(), || {
        #[cfg(target_os = "macos")]
        {
            macos::install(window, animate, dark)
        }
        #[cfg(target_os = "windows")]
        {
            let _ = (animate, dark);
            windows::install(window)
        }
        #[cfg(target_os = "linux")]
        {
            let _ = (animate, dark);
            linux::install(window)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            let _ = (window, animate, dark);
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

/// The app's theme changed to `dark` (or back): set the material's appearance
/// to match, where it is ours to set (macOS). Elsewhere the desktop picks the
/// material and the panel only changes its tint, which is painted from the
/// palette.
pub(crate) fn follow_theme(window: &Window, dark: bool) {
    #[cfg(target_os = "macos")]
    macos::set_appearance(window, dark);
    #[cfg(not(target_os = "macos"))]
    let _ = (window, dark);
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
    fn glass_hairline_contrasts_with_the_tint() {
        let dark = Chrome::glass().surfaces(&hud_palette(&Palette::dark(), true));
        assert!(dark.border.l > 0.5, "a light hairline on the dark tint");
        assert!(dark.bg.l < 0.1);
        let light = Chrome::glass().surfaces(&hud_palette(&Palette::light(), true));
        assert!(light.border.l < 0.5, "a dark hairline on the light tint");
        assert!(light.bg.l > 0.9);
    }

    #[test]
    fn the_worst_backdrop_follows_the_appearance() {
        let c = Chrome::material(true);
        assert_eq!(c.worst_backdrop(true), c.white_through);
        assert_eq!(c.worst_backdrop(false), c.grey_over_black);
        assert!(
            c.grey_over_black < 0.5,
            "a black window stays dark behind it"
        );
    }

    // quick-panel / Bright window behind the panel: a dark aqua material still
    // comes out light grey over a white window (task 7.19), so the panel darkens
    // it (and, mirrored, lightens an aqua one over a black window);
    // `hud::text_passes_over_a_white_backdrop` sets how much.
    #[test]
    fn a_macos_material_is_tinted_and_a_rimmed_one_has_no_hairline() {
        let p = hud_palette(&Palette::dark(), true);
        let glass = Chrome::material(true).surfaces(&p);
        let effect = Chrome::material(false).surfaces(&p);
        for bg in [glass.bg, effect.bg] {
            assert!(bg.a >= 0.6, "a black tint of its own: {bg:?}");
            assert!(bg.l < 0.01);
        }
        let light = Chrome::material(false).surfaces(&hud_palette(&Palette::light(), true));
        assert!(
            light.bg.a >= 0.3 && light.bg.a < effect.bg.a && light.bg.l > 0.99,
            "a white tint, lighter than the dark panel's: {:?}",
            light.bg
        );
        assert_eq!(glass.border.a, 0., "Liquid Glass draws its own edge");
        assert!(effect.border.a > 0., "a visual-effect view has none");
        assert!(Chrome::material(true).native_motion);
        assert!(!Chrome::glass().native_motion && !Chrome::solid().native_motion);
    }

    // quick-panel / Reduce transparency
    #[test]
    fn reduce_transparency_gives_the_opaque_chrome_and_installs_no_material() {
        let mut installed = false;
        let chrome = super::chrome_for(true, || {
            installed = true;
            Chrome::material(true)
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
}
