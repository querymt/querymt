//! The panel's own look (design D15): a HUD, dark whatever the app's theme or
//! the desktop's, so it does not look like a small main window.

use gpui_kit::{Hsla, black, rgb, rgba, white};

use crate::theme::Palette;

/// How much black the panel lays over a material **the desktop picks** (Mica,
/// compositor blur). Such a material can be light, so this is enough that text
/// keeps its contrast over a white backdrop.
pub(crate) const TINT: f32 = 0.58;

/// How much black the panel lays over a material that is **already dark**
/// (macOS, where the panel sets it to dark aqua). None: the material is the
/// panel's surface, and a fill of our own would only hide it.
pub(crate) const DARK_MATERIAL_TINT: f32 = 0.;

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
    let (bg, surface, card, card_hover, border, border_strong) = if glass {
        (
            black().opacity(0.24),
            hex(0x1b1e23).opacity(0.86),
            white().opacity(0.09),
            white().opacity(0.14),
            white().opacity(0.12),
            white().opacity(0.22),
        )
    } else {
        (
            hex(0x14161a),
            hex(0x1b1e23),
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
        card,
        card_hover,
        border,
        border_strong,
        text: hex(0xf5f5f5),
        muted: hex(0xd0d0d4),
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
    use super::{DARK_MATERIAL_TINT, PROGRESS_FILL_ALPHA, TINT, hud_palette, progress_colour};
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

    #[test]
    fn a_dark_material_is_left_to_show() {
        let p = hud_palette(&Palette::light(), true);
        let s = Chrome::dark_material(true).surfaces(&p);
        assert_eq!(s.bg.a, DARK_MATERIAL_TINT);
        assert!(s.bg.a < 0.2, "the material is the panel's surface");
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
