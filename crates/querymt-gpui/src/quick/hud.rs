//! The panel's own look (design D15): a HUD that follows the app's light or
//! dark appearance, in tokens of its own (glass, pills, no cards), so it does
//! not look like a small main window.

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

/// The same for a light panel: how much white it lays over those materials.
/// Lower than the dark panel's, because white over an already light blur
/// flattens it, and the light palette's text is darker (`muted` `#1f2125`), so
/// the contract holds with less. A guess to tune by eye (task 7.39); see
/// `LIQUID_GLASS_GREY_OVER_BLACK` for what it is held against.
pub(crate) const LIQUID_GLASS_LIGHT_TINT: f32 = 0.35;
pub(crate) const VFX_LIGHT_TINT: f32 = 0.35;

/// What overlays are painted with: a neutral grey (the bluish `surface` read
/// as a dark blue box over the material), see-through over a material and
/// opaque without one.
const POPOVER: u32 = 0x161616;
const LIGHT_POPOVER: u32 = 0xf6f6f6;
const POPOVER_GLASS_ALPHA: f32 = 0.72;

/// The most of a white window behind the panel that each macOS material
/// shows through, as a grey level (0 black, 1 white): what the legibility
/// contract is held against. Liquid Glass measured about 0.66 over a white
/// window on macOS 27 (task 7.19's screenshot), brighter where it bends the
/// light at the edge, hence 0.75. The visual-effect material is not measured
/// yet and is taken to be the same.
pub(crate) const LIQUID_GLASS_WHITE_THROUGH: f32 = 0.75;
pub(crate) const VFX_WHITE_THROUGH: f32 = 0.75;

/// The mirror image, for the light panel: the lightest grey the macOS
/// materials show over a **black** window, as a grey level. Not measured: the
/// dark panel's 0.66 over white mirrors to 0.34.
pub(crate) const LIQUID_GLASS_GREY_OVER_BLACK: f32 = 0.34;
pub(crate) const VFX_GREY_OVER_BLACK: f32 = 0.34;

/// Lightness an accent is raised to on dark glass, and lowered to on light
/// glass, so it reads on either.
const ACCENT_MIN_LIGHTNESS: f32 = 0.66;
const ACCENT_MAX_LIGHTNESS: f32 = 0.34;

fn hex(v: u32) -> Hsla {
    rgb(v).into()
}

fn hexa(v: u32) -> Hsla {
    rgba(v).into()
}

/// The accent of `user` made to read on the panel: lighter for dark glass (a
/// light theme's accent is darker than a dark theme's and would sink into it),
/// darker for light glass.
fn adapted(accent: Hsla, dark: bool) -> Hsla {
    Hsla {
        l: if dark {
            accent.l.max(ACCENT_MIN_LIGHTNESS)
        } else {
            accent.l.min(ACCENT_MAX_LIGHTNESS)
        },
        ..accent
    }
}

/// The panel's palette: the app's light or dark appearance (`user.dark`) in
/// the panel's own look, with the user's accent and syntax theme. Light and
/// dark are two sets of fixed tokens, neither derived from the app's colours.
///
/// Over a material (`glass`) the surface tokens are see-through, so the
/// material shows in code blocks, cards and rows instead of being covered by
/// slabs: on dark glass wells darken it a little and raised bits lighten it,
/// on light glass the other way round. Only `surface`, which popovers over the
/// conversation are painted with, stays nearly opaque. Without a material
/// every token is opaque.
pub(crate) fn hud_palette(user: &Palette, glass: bool) -> Palette {
    if user.dark {
        dark_palette(user, glass)
    } else {
        light_palette(user, glass)
    }
}

fn dark_palette(user: &Palette, glass: bool) -> Palette {
    let accent = adapted(user.accent, true);
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
        theme: user.theme,
        dark: true,
        bg,
        surface,
        popover,
        card,
        card_hover,
        selected: card,
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

fn light_palette(user: &Palette, glass: bool) -> Palette {
    let accent = adapted(user.accent, false);
    let (bg, surface, popover, card, card_hover, border, border_strong) = if glass {
        (
            black().opacity(0.05),
            hex(0xf7f7f9).opacity(0.86),
            hex(LIGHT_POPOVER).opacity(POPOVER_GLASS_ALPHA),
            white().opacity(0.28),
            white().opacity(0.40),
            black().opacity(0.12),
            black().opacity(0.24),
        )
    } else {
        (
            hex(0xf5f5f7),
            hex(0xfbfbfc),
            hex(LIGHT_POPOVER),
            hex(0xeceef1),
            hex(0xe3e6ea),
            hex(0xd5d8dd),
            hex(0xb4b9c1),
        )
    };
    Palette {
        theme: user.theme,
        dark: false,
        bg,
        surface,
        popover,
        card,
        card_hover,
        selected: card,
        border,
        border_strong,
        text: hex(0x15171a),
        muted: hex(0x1f2125),
        dim: hex(0x5c6068),
        accent,
        accent_hover: Hsla {
            l: (accent.l - 0.06).max(0.),
            ..accent
        },
        accent_dim: accent.opacity(0.16),
        on_accent: hex(0xffffff),
        success: hex(0x15803d),
        running: hex(0x8a5a00),
        danger: hex(0xb91c1c),
        warn: hex(0xa14a08),
        status_muted: hex(0x5f6368),
        chip: hexa(0x0000000a),
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use super::{TINT, hud_palette};
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

    // quick-panel / Overlays sit on the panel
    #[test]
    fn the_popover_is_neutral_see_through_over_a_material_and_opaque_without() {
        for user in [Palette::light(), Palette::dark()] {
            let (glass, solid) = (hud_palette(&user, true), hud_palette(&user, false));
            for p in [glass.popover, solid.popover] {
                assert!(p.s < 0.05, "no blue cast: {p:?}");
                assert_eq!(p.l < 0.5, user.dark, "the panel's own appearance: {p:?}");
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

    // quick-panel / Light app theme, Dark app theme
    #[test]
    fn the_palette_follows_the_apps_appearance() {
        for user in [Palette::light(), Palette::dark()] {
            for glass in [false, true] {
                let p = hud_palette(&user, glass);
                assert_eq!(p.dark, user.dark);
                assert_eq!(p.theme, user.theme, "its syntax colours follow the theme");
                assert_eq!(luminance(p.text) > 0.5, user.dark, "text opposes the glass");
            }
            let p = hud_palette(&user, false);
            assert_eq!(luminance(p.bg) < 0.05, user.dark, "{user:?}");
            assert_eq!(luminance(p.surface) > 0.5, !user.dark);
        }
        // Two looks of its own: neither is the app's palette.
        for user in [Palette::light(), Palette::dark()] {
            let p = hud_palette(&user, false);
            assert_ne!(
                (p.bg, p.surface, p.card),
                (user.bg, user.surface, user.card)
            );
        }
    }

    #[test]
    fn the_accent_is_adapted_to_the_glass() {
        // Dark glass: lightened, still the user's colour.
        let light_user = Palette::light();
        let on_dark = hud_palette(
            &Palette {
                dark: true,
                ..light_user
            },
            true,
        );
        assert!(on_dark.accent.l >= 0.66 && on_dark.accent.l >= light_user.accent.l);
        assert_eq!(on_dark.accent.h, light_user.accent.h);
        // An accent that is light enough stays as it is.
        let dark = Palette::dark();
        assert_eq!(hud_palette(&dark, true).accent.l, dark.accent.l.max(0.66));
        // Light glass: darkened enough for white text on it.
        let on_light = hud_palette(
            &Palette {
                dark: false,
                ..dark
            },
            true,
        );
        assert!(on_light.accent.l <= 0.34 && on_light.accent.l <= dark.accent.l);
        assert_eq!(on_light.accent.h, dark.accent.h);
        assert!(contrast(on_light.on_accent, on_light.accent) >= 4.5);
        // Links and the user's accent read on the light surface too.
        let surface = over(on_light.surface, gpui_kit::white());
        assert!(contrast(on_light.accent, surface) >= 4.5);
    }

    #[test]
    fn the_tint_keeps_text_readable_over_the_worst_backdrop() {
        // A material the desktop picks can be white (or black) behind the panel.
        for user in [Palette::dark(), Palette::light()] {
            let p = hud_palette(&user, true);
            let worst = if user.dark {
                gpui_kit::white()
            } else {
                gpui_kit::black()
            };
            let tint = Chrome::glass().surfaces(&p).bg;
            assert!(
                (tint.a - TINT).abs() < 1e-6,
                "the tint is the root's colour"
            );
            let backdrop = over(tint, worst);
            let ratio = contrast(p.text, backdrop);
            assert!(
                ratio > 4.5,
                "dark {}: text over its worst backdrop: {ratio}",
                user.dark
            );
        }
    }

    /// Every kind of chrome the panel can have, with the palette it paints.
    fn every_chrome() -> Vec<(&'static str, Chrome)> {
        vec![
            ("solid", Chrome::solid()),
            ("desktop's material", Chrome::glass()),
            ("visual-effect material", Chrome::material(false)),
            ("liquid glass", Chrome::material(true)),
        ]
    }

    // quick-panel / Bright window behind the panel: the essential text keeps
    // 4.5:1 over the worst backdrop the material lets through.
    #[test]
    fn text_passes_over_the_worst_backdrop() {
        for user in [Palette::light(), Palette::dark()] {
            for (kind, chrome) in every_chrome() {
                let p = hud_palette(&user, chrome.glass);
                // What shows through at worst (white behind a dark panel, black
                // behind a light one), then the panel's tint over it.
                let backdrop = Hsla {
                    a: 1.,
                    ..gpui_kit::hsla(0., 0., chrome.worst_backdrop(p.dark), 1.)
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
        for user in [Palette::dark(), Palette::light()] {
            let p = hud_palette(&user, true);
            // Wells and raised bits let the material show.
            for colour in [p.bg, p.card, p.card_hover, p.border, p.chip] {
                assert!(colour.a < 0.7, "{colour:?}");
            }
            // Popovers lie over the conversation and must hide it.
            assert!(p.surface.a > 0.8);
            if user.dark {
                assert!(
                    p.bg.l < 0.1 && p.card.l > 0.9,
                    "wells darken, cards lighten"
                );
            } else {
                assert!(
                    p.bg.l < 0.1 && p.card.l > 0.9,
                    "light glass: cards are white"
                );
            }
        }
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
