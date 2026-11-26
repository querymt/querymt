//! The panel's own look (design D15): a HUD, dark whatever the app's theme or
//! the desktop's, so it does not look like a small main window.

use gpui_kit::{Hsla, black, rgb, rgba, white};

use crate::theme::Palette;

/// How much black the panel lays over a material **the desktop picks** (Mica,
/// compositor blur). Such a material can be light, so this is enough that text
/// keeps its contrast over a white backdrop.
pub(crate) const TINT: f32 = 0.68;

/// How much black the panel lays over Liquid Glass and over the macOS
/// visual-effect material. Set to dark aqua, they still come out light grey
/// over a bright window, and our text, drawn by GPUI, cannot adapt to that as
/// AppKit's own text does: the panel keeps its own background dark. 0.6 is
/// about the least that keeps `muted` on a chip at 4.5:1 over a white window
/// (0.55 gives 4.46:1).
pub(crate) const LIQUID_GLASS_TINT: f32 = 0.6;
pub(crate) const VFX_TINT: f32 = 0.6;

/// What overlays are painted with: a neutral grey (the bluish `surface` read
/// as a dark blue box over the material), see-through over a material and
/// opaque without one.
const POPOVER: u32 = 0x161616;
const POPOVER_GLASS_ALPHA: f32 = 0.72;

/// The most of a white window behind the panel that each macOS material
/// shows through, as a grey level (0 black, 1 white): what the legibility
/// contract is held against. Liquid Glass measured about 0.66 over a white
/// window on macOS 27 (task 7.19's screenshot), brighter where it bends the
/// light at the edge, hence 0.75. The visual-effect material is not measured
/// yet and is taken to be the same.
pub(crate) const LIQUID_GLASS_WHITE_THROUGH: f32 = 0.75;
pub(crate) const VFX_WHITE_THROUGH: f32 = 0.75;

/// Lightness an accent is raised to, so it reads on dark glass.
const ACCENT_MIN_LIGHTNESS: f32 = 0.66;

fn hex(v: u32) -> Hsla {
    rgb(v).into()
}

fn hexa(v: u32) -> Hsla {
    rgba(v).into()
}

/// The accent of `user` made light enough for dark glass; a light theme's
/// accent is darker than a dark theme's, and would sink into the panel.
fn lightened(accent: Hsla) -> Hsla {
    Hsla {
        l: accent.l.max(ACCENT_MIN_LIGHTNESS),
        ..accent
    }
}

/// The panel's palette: always `dark`, with the user's accent. `user` is the
/// app's palette, read only for that accent.
///
/// Over a material (`glass`) the surface tokens are see-through, so the
/// material shows in code blocks, cards and rows instead of being covered by
/// slabs: wells darken it a little, raised bits lighten it. Only `surface`,
/// which popovers over the conversation are painted with, stays nearly opaque.
/// Without a material every token is opaque.
pub(crate) fn hud_palette(user: &Palette, glass: bool) -> Palette {
    let accent = lightened(user.accent);
    let (bg, surface, popover, card, card_hover, border, border_strong) = if glass {
        (
            black().opacity(0.24),
            hex(0x1b1e23).opacity(0.86),
            hex(POPOVER).opacity(POPOVER_GLASS_ALPHA),
            white().opacity(0.09),
            white().opacity(0.14),
            white().opacity(0.12),
            white().opacity(0.22),
        )
    } else {
        (
            hex(0x14161a),
            hex(0x1b1e23),
            hex(POPOVER),
            hex(0x22252b),
            hex(0x2a2e35),
            hex(0x30343b),
            hex(0x454a53),
        )
    };
    Palette {
        dark: true,
        bg,
        surface,
        popover,
        card,
        card_hover,
        border,
        border_strong,
        text: hex(0xf5f5f5),
        muted: hex(0xdedee2),
        dim: hex(0xa4a8af),
        accent,
        accent_hover: Hsla {
            l: (accent.l + 0.08).min(1.),
            ..accent
        },
        // Behind the user's messages: the accent at a strength that shows on
        // glass without a bright block.
        accent_dim: accent.opacity(0.24),
        on_accent: hex(0x14161a),
        success: hex(0x86efac),
        running: hex(0xffd27a),
        danger: hex(0xfca5a5),
        warn: hex(0xfdba74),
        status_muted: hex(0xb8b8b8),
        chip: hexa(0xffffff14),
    }
}

/// Strength of the progress fill behind a picker row. Text lies over it,
/// so it stays faint; the cards' bar uses the colour at full strength.
pub(crate) const PROGRESS_FILL_ALPHA: f32 = 0.14;

/// Colour of a session's progress: success once finished, the accent while
/// it runs, dim otherwise. Full strength; rows lay [`PROGRESS_FILL_ALPHA`]
/// of it down.
pub(crate) fn progress_colour(p: &Palette, finished: bool, running: bool) -> Hsla {
    if finished {
        p.success
    } else if running {
        p.accent
    } else {
        p.dim
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use super::{PROGRESS_FILL_ALPHA, TINT, hud_palette, progress_colour};
    use crate::quick::chrome::Chrome;
    use crate::theme::Palette;
    use gpui_kit::{Hsla, Rgba};

    /// Linear luminance of `c` (WCAG).
    fn luminance(c: Hsla) -> f32 {
        let c = Rgba::from(c);
        let lin = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    }

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `over` laid on an opaque `under`.
    fn over(top: Hsla, under: Hsla) -> Hsla {
        let (t, u) = (Rgba::from(top), Rgba::from(under));
        Rgba {
            r: t.r * t.a + u.r * (1. - t.a),
            g: t.g * t.a + u.g * (1. - t.a),
            b: t.b * t.a + u.b * (1. - t.a),
            a: 1.,
        }
        .into()
    }

    #[test]
    fn row_text_keeps_its_contrast_over_the_progress_fill() {
        for user in [Palette::light(), Palette::dark()] {
            for glass in [false, true] {
                let p = hud_palette(&user, glass);
                for (finished, running) in [(true, false), (false, true), (false, false)] {
                    let fill = progress_colour(&p, finished, running).opacity(PROGRESS_FILL_ALPHA);
                    // The fill lies on the surface the picker is painted with.
                    let under = over(fill, Hsla { a: 1., ..p.surface });
                    for text in [p.text, p.muted] {
                        let ratio = contrast(text, under);
                        assert!(
                            ratio >= 4.5,
                            "glass={glass} finished={finished} running={running}: {ratio}"
                        );
                    }
                }
            }
        }
    }

    // quick-panel / Overlays sit on the panel
    #[test]
    fn the_popover_is_neutral_see_through_over_a_material_and_opaque_without() {
        for user in [Palette::light(), Palette::dark()] {
            let (glass, solid) = (hud_palette(&user, true), hud_palette(&user, false));
            for p in [glass.popover, solid.popover] {
                assert!(p.s < 0.05, "no blue cast: {p:?}");
                assert!(p.l < 0.2, "dark: {p:?}");
            }
            assert!(glass.popover.a < 1., "the material shows through");
            assert_eq!(solid.popover.a, 1.);
        }
        // The app's own popover is its surface.
        for p in [Palette::light(), Palette::dark()] {
            assert_eq!(p.popover, p.surface);
        }
    }

    // quick-panel / Overlays sit on the panel: the model picker, the folder
    // chooser and the command list all ask the palette whether to cast a
    // shadow.
    #[test]
    fn no_overlay_casts_a_shadow_over_a_material() {
        for user in [Palette::light(), Palette::dark()] {
            assert!(!hud_palette(&user, true).overlay_shadow());
            assert!(hud_palette(&user, false).overlay_shadow());
            assert!(user.overlay_shadow(), "the main window keeps its shadow");
        }
    }

    #[test]
    fn the_palette_is_dark_whatever_the_app_theme() {
        for user in [Palette::light(), Palette::dark()] {
            for glass in [false, true] {
                let p = hud_palette(&user, glass);
                assert!(p.dark);
                assert!(luminance(p.text) > 0.7, "light text");
            }
            let p = hud_palette(&user, false);
            assert!(luminance(p.bg) < 0.05, "a dark surface");
            assert!(luminance(p.surface) < 0.05 && luminance(p.card) < 0.05);
        }
        // The hud does not depend on the theme beyond the accent.
        let (light, dark) = (
            hud_palette(&Palette::light(), false),
            hud_palette(&Palette::dark(), false),
        );
        assert_eq!((light.bg, light.text), (dark.bg, dark.text));
    }

    #[test]
    fn the_accent_is_lightened_for_dark_glass() {
        let user = Palette::light();
        let p = hud_palette(&user, true);
        assert!(p.accent.l >= 0.66);
        assert!(p.accent.l >= user.accent.l);
        assert_eq!(p.accent.h, user.accent.h, "still the user's colour");
        // An accent that is light enough stays as it is.
        let dark = Palette::dark();
        assert_eq!(hud_palette(&dark, true).accent.l, dark.accent.l.max(0.66));
    }

    #[test]
    fn the_tint_keeps_text_readable_over_a_white_backdrop() {
        // A material the desktop picks can be white behind the panel.
        let p = hud_palette(&Palette::light(), true);
        let white = gpui_kit::white();
        let tint = Chrome::glass().surfaces(&p).bg;
        assert!(
            (tint.a - TINT).abs() < 1e-6,
            "the tint is the root's colour"
        );
        let backdrop = over(tint, white);
        let ratio = contrast(p.text, backdrop);
        assert!(ratio > 4.5, "text over a white backdrop: {ratio}");
    }

    /// Every kind of chrome the panel can have, with the palette it paints.
    fn every_chrome() -> Vec<(&'static str, Chrome)> {
        vec![
            ("solid", Chrome::solid()),
            ("desktop's material", Chrome::glass()),
            ("dark material", Chrome::dark_material(false)),
            ("liquid glass", Chrome::dark_material(true)),
        ]
    }

    // quick-panel / Bright window behind the panel: the essential text keeps
    // 4.5:1 over the worst backdrop the material lets through.
    #[test]
    fn text_passes_over_a_white_backdrop() {
        for user in [Palette::light(), Palette::dark()] {
            for (kind, chrome) in every_chrome() {
                let p = hud_palette(&user, chrome.glass);
                // What shows through, then the panel's tint over it.
                let backdrop = Hsla {
                    a: 1.,
                    ..gpui_kit::hsla(0., 0., chrome.white_through, 1.)
                };
                let surface = over(chrome.surfaces(&p).bg, backdrop);
                // What labels sit on: the surface, a chip, the user's pill.
                let grounds = [
                    ("surface", surface),
                    ("chip", over(p.chip, surface)),
                    ("user message", over(p.accent_dim, surface)),
                ];
                for (ground, under) in grounds {
                    for (name, colour) in [("text", p.text), ("muted", p.muted)] {
                        // The user's pill holds message text, never labels.
                        if ground == "user message" && name == "muted" {
                            continue;
                        }
                        let ratio = contrast(colour, under);
                        assert!(ratio >= 4.5, "{kind}: {name} on {ground} is {ratio:.2}:1");
                    }
                }
            }
        }
    }

    #[test]
    fn over_a_material_the_surfaces_are_see_through() {
        let p = hud_palette(&Palette::dark(), true);
        // Wells and raised bits let the material show.
        for colour in [p.bg, p.card, p.card_hover, p.border, p.chip] {
            assert!(colour.a < 0.5, "{colour:?}");
        }
        // Popovers lie over the conversation and must hide it.
        assert!(p.surface.a > 0.8);
        assert!(
            p.bg.l < 0.1 && p.card.l > 0.9,
            "wells darken, cards lighten"
        );
    }

    #[test]
    fn the_opaque_fallback_has_alpha_one() {
        for user in [Palette::light(), Palette::dark()] {
            let p = hud_palette(&user, false);
            let s = Chrome::solid().surfaces(&p);
            for colour in [s.bg, s.border, p.surface, p.card, p.card_hover] {
                assert_eq!(colour.a, 1.0);
            }
            assert!(contrast(p.text, s.bg) > 12.);
        }
    }
}
