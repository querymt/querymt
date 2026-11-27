//! Accent presets and the rule that applies one over a theme (design D6).

use gpui_kit::{Hsla, Rgba, rgb};

use super::{Palette, ThemeId};

/// Accent presets (same set as the Svelte app): id, label, dark, light.
pub const ACCENTS: [(&str, &str, u32, u32); 9] = [
    ("blue", "Blue", 0x60a5fa, 0x3b82f6),
    ("teal", "Teal", 0x2dd4bf, 0x14b8a6),
    ("green", "Green", 0x4ade80, 0x22c55e),
    ("amber", "Amber", 0xfacc15, 0xeab308),
    ("orange", "Orange", 0xfb923c, 0xf97316),
    ("red", "Red", 0xf87171, 0xef4444),
    ("pink", "Pink", 0xf472b6, 0xec4899),
    ("purple", "Purple", 0xc084fc, 0xa855f7),
    ("slate", "Slate", 0x94a3b8, 0x64748b),
];

/// The least contrast a label may have with a picked preset's fill (WCAG's
/// minimum for large text and non-text content).
pub const MIN_LABEL_CONTRAST: f32 = 3.0;

/// An index into [`ACCENTS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset(usize);

impl Preset {
    /// The accent of QueryMT Dark and Light.
    pub const ORANGE: Preset = Preset(4);

    pub fn all() -> impl Iterator<Item = Preset> {
        (0..ACCENTS.len()).map(Preset)
    }

    pub fn id(self) -> &'static str {
        ACCENTS[self.0].0
    }

    pub fn label(self) -> &'static str {
        ACCENTS[self.0].1
    }

    /// The fill for the appearance.
    pub fn color(self, dark: bool) -> Hsla {
        let (_, _, d, l) = ACCENTS[self.0];
        rgb(if dark { d } else { l }).into()
    }

    /// Accent tint behind user message bubbles (`Palette::accent_dim`).
    pub fn dim(self, dark: bool) -> Hsla {
        self.color(dark).opacity(if dark { 0.16 } else { 0.12 })
    }

    fn hover(self, dark: bool) -> Hsla {
        let a = self.color(dark);
        Hsla {
            l: (a.l + if dark { 0.08 } else { -0.08 }).clamp(0., 1.),
            ..a
        }
    }
}

/// What the user picked for the accent: the theme's own, or a preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accent {
    Theme,
    Preset(Preset),
}

impl Accent {
    /// Reads the saved `prefs.accent`: empty and unknown ids are the theme's.
    pub fn parse(saved: &str) -> Accent {
        Preset::all()
            .find(|p| p.id() == saved)
            .map_or(Accent::Theme, Accent::Preset)
    }

    /// The string to save.
    pub fn id(self) -> &'static str {
        match self {
            Accent::Theme => "",
            Accent::Preset(p) => p.id(),
        }
    }

    /// `base` with this accent over it. `own_accent` is the preset the theme's
    /// own accent equals, if any: picking it changes nothing.
    pub fn apply_to(self, base: Palette, own_accent: Option<Preset>) -> Palette {
        let Accent::Preset(preset) = self else {
            return base;
        };
        if own_accent == Some(preset) {
            return base;
        }
        let fill = preset.color(base.dark);
        let on_accent = if contrast(base.on_accent, fill) >= MIN_LABEL_CONTRAST {
            base.on_accent
        } else {
            let black: Hsla = rgb(0x000000).into();
            let white: Hsla = rgb(0xffffff).into();
            if contrast(black, fill) >= contrast(white, fill) {
                black
            } else {
                white
            }
        };
        Palette {
            accent: fill,
            accent_hover: preset.hover(base.dark),
            accent_dim: preset.dim(base.dark),
            on_accent,
            ..base
        }
    }
}

/// The accent and the theme it is shown over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub theme: ThemeId,
    pub accent: Accent,
}

impl Appearance {
    /// The built-in theme of the mode with its own accent.
    #[cfg(test)]
    pub fn builtin(dark: bool) -> Self {
        Self {
            theme: ThemeId::of_builtin(dark),
            accent: Accent::Theme,
        }
    }

    pub fn dark(self) -> bool {
        self.theme.dark()
    }

    /// The palette this appearance paints with.
    pub fn palette(self) -> Palette {
        self.accent
            .apply_to(self.theme.palette(), self.theme.own_accent())
    }
}

fn luminance(c: Hsla) -> f32 {
    let c = Rgba::from(c);
    let lin = |v: f32| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio of two opaque colours, 1 to 21.
pub fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}
