//! QueryMT design tokens (from `querymt-desktop/src/app.css`) applied to the
//! gpui-component theme, plus a `Palette` global for app-specific colors
//! (status tones, accent tints) that the component theme has no slot for.

mod accent;
mod catalogue;
mod fonts;

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::{App, Global, Hsla, px, rgb, rgba};

pub use accent::{Accent, Appearance, Preset};
pub use catalogue::{ThemeId, syntax};
#[cfg(test)]
pub use fonts::resolve as resolve_typography;
pub use fonts::{
    DEFAULT_MONO_FAMILY, DEFAULT_MONO_SIZE, DEFAULT_UI_FAMILY, FontCatalog, FontPrefs, Installed,
    MONO_SIZE_RANGE, ThemeFonts, Typography, UI_SIZE_RANGE, clamp_size,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// The theme this palette belongs to: its syntax colours come with it.
    pub theme: ThemeId,
    pub dark: bool,
    pub bg: Hsla,
    pub surface: Hsla,
    /// What menus and pickers over the page are painted with. The same as
    /// `surface` in the app; the quick panel's own is neutral and, over its
    /// material, see-through (quick-panel / Overlays sit on the panel).
    pub popover: Hsla,
    pub card: Hsla,
    pub card_hover: Hsla,
    /// The highlighted row of a list or menu (the kit's `accent`). The theme's
    /// own selection colour; the built-ins' `card` (design D4).
    pub selected: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub dim: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_dim: Hsla,
    pub on_accent: Hsla,
    pub success: Hsla,
    pub running: Hsla,
    pub danger: Hsla,
    pub warn: Hsla,
    pub status_muted: Hsla,
    pub chip: Hsla,
}

impl Global for Palette {}

fn hex(v: u32) -> Hsla {
    rgb(v).into()
}
fn hexa(v: u32) -> Hsla {
    rgba(v).into()
}

impl Palette {
    /// Overlays (menus, pickers) cast a shadow. Not over the quick panel's
    /// material: the overlay is part of the panel's surface there, and a
    /// shadow on a see-through card would show as a dark smear (design D17).
    pub fn overlay_shadow(&self) -> bool {
        self.popover.a >= 1.
    }

    pub fn light() -> Self {
        Self {
            theme: ThemeId::QMT_LIGHT,
            dark: false,
            bg: hex(0xf6f5f4),
            surface: hex(0xffffff),
            popover: hex(0xffffff),
            card: hex(0xf1efed),
            card_hover: hex(0xebe9e6),
            selected: hex(0xf1efed),
            border: hex(0xdeddda),
            border_strong: hex(0xc0bfbc),
            text: hex(0x241f31),
            muted: hex(0x5e5c64),
            dim: hex(0x77767b),
            accent: hex(0xf97316),
            accent_hover: hex(0xea580c),
            accent_dim: hexa(0xf973161f),
            on_accent: hex(0xffffff),
            success: hex(0x1a7f47),
            running: hex(0x9a6700),
            danger: hex(0xc01c28),
            warn: hex(0xa45d00),
            status_muted: hex(0x5e5c64),
            chip: hexa(0x5e5c6414),
        }
    }

    pub fn dark() -> Self {
        Self {
            theme: ThemeId::QMT_DARK,
            dark: true,
            bg: hex(0x1b1b1b),
            surface: hex(0x202020),
            popover: hex(0x202020),
            card: hex(0x292929),
            card_hover: hex(0x303030),
            selected: hex(0x292929),
            border: hex(0x383838),
            border_strong: hex(0x505050),
            text: hex(0xf5f5f5),
            muted: hex(0xc4c4c4),
            dim: hex(0x999999),
            accent: hex(0xfb923c),
            accent_hover: hex(0xfdba74),
            accent_dim: hexa(0xfb923c29),
            on_accent: hex(0x1b1b1b),
            success: hex(0x86efac),
            running: hex(0xffd27a),
            danger: hex(0xfca5a5),
            warn: hex(0xfdba74),
            status_muted: hex(0xb8b8b8),
            chip: hexa(0xffffff12),
        }
    }
}

pub fn palette(cx: &App) -> &Palette {
    cx.global::<Palette>()
}

/// The type the theme `id` shows when the user has chosen nothing: its own
/// settings over the app defaults. What "Theme default" means in settings.
pub fn theme_typography(id: ThemeId, cx: &mut App) -> Typography {
    fonts::resolve_installed(&FontPrefs::default(), &id.fonts(), false, cx)
}

/// The type in force: the kit theme's fonts, as [`apply_with`] set them.
pub fn typography(cx: &App) -> Typography {
    let t = Theme::global(cx);
    Typography {
        ui_family: t.font_family.clone(),
        ui_size: f32::from(t.font_size),
        mono_family: t.mono_font_family.clone(),
        mono_size: f32::from(t.mono_font_size),
    }
}

/// [`apply_with`] with no user font choices: the theme's own type over the
/// app defaults. The app itself always has choices to pass (the tests and the
/// film scenes do not).
#[cfg(test)]
pub fn apply(appearance: Appearance, cx: &mut App) {
    apply_with(appearance, &FontPrefs::default(), cx);
}

/// Show `appearance`: set the palette, install the theme's kit config, then
/// write the app's own tokens over it (design D7). Each of the four type
/// fields is the user's setting in `fonts`, else the theme's, else the app
/// default; families that are not installed are skipped (see [`fonts`]).
pub fn apply_with(appearance: Appearance, fonts: &FontPrefs, cx: &mut App) {
    apply_resolved(appearance, fonts, &appearance.theme.fonts(), cx);
}

/// [`apply_with`] with the theme's type given (tests name fonts no bundled
/// theme file sets).
fn apply_resolved(
    appearance: Appearance,
    fonts: &FontPrefs,
    theme_fonts: &ThemeFonts,
    cx: &mut App,
) {
    let id = appearance.theme;
    let typography = fonts::resolve_installed(fonts, theme_fonts, true, cx);
    let dark = appearance.dark();
    let p = appearance.palette();
    cx.set_global(p);
    // The first call creates the kit theme; later ones only switch its mode.
    Theme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    let config = id.config().unwrap_or_else(|| {
        let registry = ThemeRegistry::global(cx);
        if dark {
            registry.default_dark_theme().clone()
        } else {
            registry.default_light_theme().clone()
        }
    });
    // Its own update: a mode change with the same config pointer makes the
    // kit reload the config over whatever the same closure wrote after it.
    Theme::update(cx, |t| t.apply_config(&config));
    let syntax = id.syntax();
    Theme::update(cx, |t| {
        t.font_family = typography.ui_family.clone();
        t.font_size = px(typography.ui_size);
        t.mono_font_family = typography.mono_family.clone();
        t.mono_font_size = px(typography.mono_size);
        t.radius = px(8.);
        t.radius_lg = px(12.);
        t.window_radius = if cfg!(target_os = "linux") {
            px(12.)
        } else {
            px(0.)
        };

        t.background = p.bg;
        t.foreground = p.text;
        t.border = p.border;
        t.input = p.border;
        t.ring = p.accent;
        t.caret = p.accent;
        t.selection = p.accent_dim;
        t.muted = p.card;
        t.muted_foreground = p.muted;
        t.link = p.accent;
        t.link_hover = p.accent_hover;

        t.primary = p.accent;
        t.primary_hover = p.accent_hover;
        t.primary_active = p.accent_hover;
        t.primary_foreground = p.on_accent;
        t.button_primary = p.accent;
        t.button_primary_hover = p.accent_hover;
        t.button_primary_active = p.accent_hover;
        t.button_primary_foreground = p.on_accent;

        t.secondary = p.card;
        t.secondary_hover = p.card_hover;
        t.secondary_active = p.card_hover;
        t.secondary_foreground = p.text;
        t.button = p.surface;
        t.button_hover = p.card;
        t.button_active = p.card_hover;
        t.button_foreground = p.text;

        t.accent = p.selected;
        t.accent_foreground = p.text;
        t.popover = p.popover;
        t.popover_foreground = p.text;
        t.list_hover = p.card;
        t.list_active = p.accent_dim;
        t.list_active_border = p.accent;

        t.sidebar = p.surface;
        t.sidebar_foreground = p.text;
        t.sidebar_border = p.border;
        t.sidebar_accent = p.selected;
        t.sidebar_accent_foreground = p.text;
        t.sidebar_primary = p.accent;
        t.sidebar_primary_foreground = p.on_accent;

        t.title_bar = p.bg;
        t.title_bar_border = p.bg;
        t.window_border = p.border;

        t.danger = p.danger;
        t.success = p.success;
        t.warning = p.warn;
        t.highlight_theme = syntax;
    });
    crate::highlight::install(cx);
}

#[cfg(test)]
mod tests;
