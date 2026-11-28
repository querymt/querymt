//! The themes the app offers: the two built-in QueryMT themes and the bundled
//! gpui-kit theme files, each resolved once into a palette and syntax colours
//! (design D1, D4, D5).

use std::{rc::Rc, sync::Arc};

use gpui_kit::component::{Theme, ThemeConfig, ThemeSet, highlighter::HighlightTheme};
use gpui_kit::{Hsla, Rgba, SharedString};

use super::{Palette, Preset, ThemeFonts};

/// The bundled theme files: the vendored kit files (`assets/themes/kit/NOTICE.md`)
/// and the three converted from Zed (`assets/themes/extra/NOTICE.md`, design D11).
pub(super) const FILES: [&str; 24] = [
    include_str!("../../assets/themes/kit/adventure.json"),
    include_str!("../../assets/themes/kit/alduin.json"),
    include_str!("../../assets/themes/kit/asciinema.json"),
    include_str!("../../assets/themes/kit/aurora.json"),
    include_str!("../../assets/themes/kit/ayu.json"),
    include_str!("../../assets/themes/kit/catppuccin.json"),
    include_str!("../../assets/themes/kit/everforest.json"),
    include_str!("../../assets/themes/kit/fahrenheit.json"),
    include_str!("../../assets/themes/kit/flexoki.json"),
    include_str!("../../assets/themes/kit/gruvbox.json"),
    include_str!("../../assets/themes/kit/harper.json"),
    include_str!("../../assets/themes/kit/hybrid.json"),
    include_str!("../../assets/themes/kit/jellybeans.json"),
    include_str!("../../assets/themes/kit/kibble.json"),
    include_str!("../../assets/themes/kit/macos-classic.json"),
    include_str!("../../assets/themes/kit/mellifluous.json"),
    include_str!("../../assets/themes/kit/molokai.json"),
    include_str!("../../assets/themes/kit/solarized.json"),
    include_str!("../../assets/themes/kit/spaceduck.json"),
    include_str!("../../assets/themes/kit/tokyonight.json"),
    include_str!("../../assets/themes/kit/twilight.json"),
    include_str!("../../assets/themes/extra/ayu-mirage.json"),
    include_str!("../../assets/themes/extra/gruvbox-variants.json"),
    include_str!("../../assets/themes/extra/one.json"),
];

/// A theme of the catalogue. An index into a per-process list: valid for the
/// process only and never saved (the saved value is [`ThemeId::key`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThemeId(usize);

struct Entry {
    key: SharedString,
    label: SharedString,
    dark: bool,
    /// `None` for the built-ins: the kit's own default config.
    config: Option<Rc<ThemeConfig>>,
    palette: Palette,
    /// The preset the theme's own accent equals (the built-ins' orange).
    own_accent: Option<Preset>,
    syntax: Arc<HighlightTheme>,
    /// The type the theme file names (`font.*`, `mono_font.*`), if any.
    fonts: ThemeFonts,
}

thread_local! {
    // Not a `static`: the entries hold `Rc` configs, and everything that reads
    // the catalogue runs on the main thread.
    static CATALOGUE: Vec<Entry> = load();
}

fn with<R>(f: impl FnOnce(&[Entry]) -> R) -> R {
    CATALOGUE.with(|entries| f(entries))
}

impl ThemeId {
    pub const QMT_DARK: ThemeId = ThemeId(0);
    pub const QMT_LIGHT: ThemeId = ThemeId(1);

    pub fn of_builtin(dark: bool) -> ThemeId {
        if dark {
            Self::QMT_DARK
        } else {
            Self::QMT_LIGHT
        }
    }

    /// The theme saved as `key`, or the built-in theme of `dark` when the key
    /// is empty, unknown or names a theme of the other appearance.
    pub fn resolve(saved: &str, dark: bool) -> ThemeId {
        with(|entries| {
            entries
                .iter()
                .position(|e| e.dark == dark && *e.key == *saved)
                .map_or(Self::of_builtin(dark), ThemeId)
        })
    }

    /// The rows of a theme select: the built-in theme, then the bundled
    /// themes of that appearance.
    pub fn of_appearance(dark: bool) -> Vec<ThemeId> {
        with(|entries| {
            entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.dark == dark)
                .map(|(i, _)| ThemeId(i))
                .collect()
        })
    }

    pub fn key(self) -> SharedString {
        with(|e| e[self.0].key.clone())
    }

    pub fn label(self) -> SharedString {
        with(|e| e[self.0].label.clone())
    }

    pub fn dark(self) -> bool {
        with(|e| e[self.0].dark)
    }

    /// The theme's own accent.
    pub fn accent(self) -> Hsla {
        with(|e| e[self.0].palette.accent)
    }

    /// Background, text, accent and string colour: the swatches of a select row.
    pub fn palette_swatches(self) -> [Hsla; 4] {
        with(|e| {
            let e = &e[self.0];
            let string = e
                .syntax
                .style("string")
                .and_then(|s| s.color)
                .unwrap_or(e.palette.text);
            [e.palette.bg, e.palette.text, e.palette.accent, string]
        })
    }

    pub(super) fn palette(self) -> Palette {
        with(|e| e[self.0].palette)
    }

    pub(super) fn own_accent(self) -> Option<Preset> {
        with(|e| e[self.0].own_accent)
    }

    pub(super) fn config(self) -> Option<Rc<ThemeConfig>> {
        with(|e| e[self.0].config.clone())
    }

    pub fn syntax(self) -> Arc<HighlightTheme> {
        with(|e| e[self.0].syntax.clone())
    }

    /// The families and sizes the theme file names; empty for the built-ins
    /// and for files that do not set them. [`super::apply`] decides what
    /// is shown (user setting, then this, then the app default).
    pub fn fonts(self) -> ThemeFonts {
        with(|e| e[self.0].fonts.clone())
    }
}

/// The syntax colours of the palette's theme.
pub fn syntax(p: &Palette) -> Arc<HighlightTheme> {
    p.theme.syntax()
}

/// What the app keeps for itself, whatever a theme file says: its radii and
/// shadow setting. The highlight block moves to the entry, and so do the
/// four type settings (`font.*`, `mono_font.*`): they are taken out of the
/// config so that `apply_config` does not write them, and [`super::apply`]
/// resolves them against the user's setting instead.
pub(super) fn prepare(
    mut config: ThemeConfig,
) -> (Rc<ThemeConfig>, Arc<HighlightTheme>, ThemeFonts) {
    let fonts = ThemeFonts {
        ui_family: config.font_family.take().filter(|f| !f.trim().is_empty()),
        ui_size: config.font_size.take(),
        mono_family: config
            .mono_font_family
            .take()
            .filter(|f| !f.trim().is_empty()),
        mono_size: config.mono_font_size.take(),
    };
    config.radius = None;
    config.radius_lg = None;
    config.window_radius = None;
    config.shadow = None;
    let base = if config.mode.is_dark() {
        HighlightTheme::default_dark()
    } else {
        HighlightTheme::default_light()
    };
    let syntax = match config.highlight.take() {
        Some(style) => {
            let mut theme = (*base).clone();
            theme.name = config.name.to_string();
            theme.appearance = config.mode;
            theme.style = style;
            Arc::new(theme)
        }
        None => base,
    };
    (Rc::new(config), syntax, fonts)
}

/// Every theme of the bundled files, as the files define them.
pub(super) fn parse_files() -> Vec<ThemeConfig> {
    FILES
        .iter()
        .flat_map(|file| {
            serde_json::from_str::<ThemeSet>(file)
                .expect("bundled theme file parses")
                .themes
        })
        .collect()
}

pub(super) fn kebab(name: &str) -> String {
    let mut key = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            key.push(c.to_ascii_lowercase());
        } else if !key.ends_with('-') && !key.is_empty() {
            key.push('-');
        }
    }
    key.trim_end_matches('-').to_string()
}

fn builtin(key: &str, label: &str, palette: Palette, syntax: Arc<HighlightTheme>) -> Entry {
    Entry {
        key: key.to_string().into(),
        label: label.to_string().into(),
        dark: palette.dark,
        config: None,
        palette,
        own_accent: Some(Preset::ORANGE),
        syntax,
        fonts: ThemeFonts::default(),
    }
}

fn load() -> Vec<Entry> {
    let mut kit: Vec<ThemeConfig> = parse_files();
    kit.sort_by_key(|c| c.name.to_lowercase());
    let mut entries = vec![
        builtin(
            "qmt-dark",
            "QueryMT Dark",
            Palette::dark(),
            HighlightTheme::default_dark(),
        ),
        builtin(
            "qmt-light",
            "QueryMT Light",
            Palette::light(),
            HighlightTheme::default_light(),
        ),
    ];
    for dark in [true, false] {
        for config in kit.iter().filter(|c| c.mode.is_dark() == dark) {
            let id = ThemeId(entries.len());
            let (rc, syntax, fonts) = prepare(config.clone());
            entries.push(Entry {
                key: kebab(&config.name).into(),
                label: config.name.clone(),
                dark,
                palette: palette_of(&rc, id),
                config: Some(rc),
                own_accent: None,
                syntax,
                fonts,
            });
        }
    }
    entries
}

/// `fg` over `bg`, so a surface token never lets the page show through.
fn opaque(fg: Hsla, bg: Hsla) -> Hsla {
    if fg.a >= 1. {
        return fg;
    }
    let (f, b) = (Rgba::from(fg), Rgba::from(bg));
    let mix = |f: f32, b: f32| f * fg.a + b * (1. - fg.a);
    Rgba {
        r: mix(f.r, b.r),
        g: mix(f.g, b.g),
        b: mix(f.b, b.b),
        a: 1.,
    }
    .into()
}

/// The per-channel sRGB average of two opaque colours.
fn midpoint(a: Hsla, b: Hsla) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    Rgba {
        r: (a.r + b.r) / 2.,
        g: (a.g + b.g) / 2.,
        b: (a.b + b.b) / 2.,
        a: 1.,
    }
    .into()
}

/// The app palette of a kit theme (design D4): the config resolved on a
/// scratch kit theme, so the kit's own fallbacks apply, then mapped to tokens.
pub(super) fn palette_of(config: &Rc<ThemeConfig>, id: ThemeId) -> Palette {
    let mut kit = Theme::default();
    kit.apply_config(config);
    let dark = config.mode.is_dark();
    let bg = opaque(kit.background, kit.background);
    let o = |c: Hsla| opaque(c, bg);
    let text = o(kit.foreground);
    let dim = o(kit.muted_foreground);
    let mid = midpoint(text, dim);
    let accent = o(kit.primary);
    Palette {
        theme: id,
        dark,
        bg,
        surface: o(kit.sidebar),
        popover: o(kit.popover),
        card: o(kit.secondary),
        card_hover: o(kit.secondary_hover),
        selected: o(kit.accent),
        border: o(kit.border),
        border_strong: o(kit.input),
        text,
        muted: mid,
        dim,
        accent,
        accent_hover: o(kit.primary_hover),
        accent_dim: accent.opacity(if dark { 0.16 } else { 0.12 }),
        on_accent: o(kit.primary_foreground),
        success: o(kit.green),
        running: o(kit.yellow),
        danger: o(kit.red),
        warn: o(kit.yellow),
        status_muted: mid,
        chip: text.opacity(0.07),
    }
}
