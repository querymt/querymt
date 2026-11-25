//! QueryMT design tokens (from `querymt-desktop/src/app.css`) applied to the
//! gpui-component theme, plus a `Palette` global for app-specific colors
//! (status tones, accent tints) that the component theme has no slot for.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, px, rgb, rgba};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub bg: Hsla,
    pub surface: Hsla,
    pub card: Hsla,
    pub card_hover: Hsla,
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
    pub fn light() -> Self {
        Self {
            dark: false,
            bg: hex(0xf6f5f4),
            surface: hex(0xffffff),
            card: hex(0xf1efed),
            card_hover: hex(0xebe9e6),
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
            dark: true,
            bg: hex(0x1b1b1b),
            surface: hex(0x202020),
            card: hex(0x292929),
            card_hover: hex(0x303030),
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

pub fn accent_color(id: &str, dark: bool) -> Hsla {
    let (_, _, d, l) = ACCENTS
        .iter()
        .find(|a| a.0 == id)
        .copied()
        .unwrap_or(ACCENTS[4]);
    hex(if dark { d } else { l })
}

/// Accent tint behind user message bubbles (`Palette::accent_dim`).
pub fn accent_dim(id: &str, dark: bool) -> Hsla {
    accent_color(id, dark).opacity(if dark { 0.16 } else { 0.12 })
}

/// Switch light/dark/accent and re-apply QueryMT colors over the component theme.
pub fn apply(dark: bool, accent: &str, cx: &mut App) {
    let mut p = if dark {
        Palette::dark()
    } else {
        Palette::light()
    };
    if !accent.is_empty() && accent != "orange" {
        let a = accent_color(accent, dark);
        p.accent = a;
        p.accent_hover = Hsla {
            l: (a.l + if dark { 0.08 } else { -0.08 }).clamp(0., 1.),
            ..a
        };
        p.accent_dim = accent_dim(accent, dark);
    }
    cx.set_global(p);
    Theme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    Theme::update(cx, |t| {
        t.font_family = "IBM Plex Sans".into();
        t.mono_font_family = "IBM Plex Mono".into();
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

        t.accent = p.card;
        t.accent_foreground = p.text;
        t.popover = p.surface;
        t.popover_foreground = p.text;
        t.list_hover = p.card;
        t.list_active = p.accent_dim;
        t.list_active_border = p.accent;

        t.sidebar = p.surface;
        t.sidebar_foreground = p.text;
        t.sidebar_border = p.border;
        t.sidebar_accent = p.card;
        t.sidebar_accent_foreground = p.text;
        t.sidebar_primary = p.accent;
        t.sidebar_primary_foreground = p.on_accent;

        t.title_bar = p.bg;
        t.title_bar_border = p.bg;
        t.window_border = p.border;

        t.danger = p.danger;
        t.success = p.success;
        t.warning = p.warn;
    });
    crate::highlight::install(cx);
}
