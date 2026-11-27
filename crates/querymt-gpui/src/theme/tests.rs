//! Theme catalogue, conversion, accent rule and `apply`.

use std::sync::Arc;

use gpui_kit::component::{Theme, ThemeConfig, highlighter::HighlightTheme, try_parse_color};
use gpui_kit::{Hsla, Rgba, TestAppContext, px, rgb};
use serde_json::Value;

use super::accent::{MIN_LABEL_CONTRAST, contrast};
use super::catalogue::{FILES, kebab, parse_files, prepare};
use super::*;

const DARK: [&str; 29] = [
    "QueryMT Dark",
    "Adventure",
    "Adventure Time",
    "Alduin",
    "Asciinema",
    "Ayu Dark",
    "Ayu Mirage",
    "Catppuccin Frappe",
    "Catppuccin Macchiato",
    "Catppuccin Mocha",
    "Everforest Dark",
    "Fahrenheit",
    "Flexoki Dark",
    "Gruvbox Dark",
    "Gruvbox Dark Soft",
    "Harper",
    "Hybrid Dark",
    "Jellybeans",
    "Kibble",
    "macOS Classic Dark",
    "Mellifluous Dark",
    "Molokai Dark",
    "One Dark",
    "Solarized Dark",
    "Spaceduck",
    "Tokyo Moon",
    "Tokyo Night",
    "Tokyo Storm",
    "Twilight",
];

const LIGHT: [&str; 15] = [
    "QueryMT Light",
    "Aurora Light",
    "Ayu Light",
    "Catppuccin Latte",
    "Everforest Light",
    "Flexoki Light",
    "Gruvbox Light",
    "Gruvbox Light Hard",
    "Gruvbox Light Soft",
    "Hybrid Light",
    "macOS Classic Light",
    "Mellifluous Light",
    "Molokai Light",
    "One Light",
    "Solarized Light",
];

fn close(a: Hsla, b: Hsla) -> bool {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    (a.r - b.r).abs() < 2e-3
        && (a.g - b.g).abs() < 2e-3
        && (a.b - b.b).abs() < 2e-3
        && (a.a - b.a).abs() < 2e-3
}

#[track_caller]
fn assert_close(a: Hsla, b: Hsla, what: &str) {
    assert!(
        close(a, b),
        "{what}: {:?} vs {:?}",
        Rgba::from(a),
        Rgba::from(b)
    );
}

fn hex(v: u32) -> Hsla {
    rgb(v).into()
}

/// A theme of the catalogue by key.
fn theme(key: &str) -> ThemeId {
    [true, false]
        .into_iter()
        .map(|dark| ThemeId::resolve(key, dark))
        .find(|id| *id.key() == *key)
        .unwrap_or_else(|| panic!("no theme {key}"))
}

fn bundled() -> Vec<ThemeId> {
    [true, false]
        .into_iter()
        .flat_map(ThemeId::of_appearance)
        .filter(|id| id.config().is_some())
        .collect()
}

/// The theme's block of a vendored file, as written.
fn raw(key: &str) -> Value {
    FILES
        .iter()
        .flat_map(|file| {
            let value: Value = serde_json::from_str(file).expect("valid json");
            value["themes"].as_array().expect("themes").clone()
        })
        .find(|t| kebab(t["name"].as_str().expect("name")) == key)
        .unwrap_or_else(|| panic!("no file for {key}"))
}

/// A colour of the file: the first stop of a gradient.
fn raw_color(key: &str, field: &str) -> Hsla {
    let value = raw(key);
    let text = value["colors"][field]
        .as_str()
        .unwrap_or_else(|| panic!("{key} has no {field}"))
        .to_string();
    let text = match text.find('#') {
        Some(at) if text.starts_with("linear-gradient") => text[at..at + 7].to_string(),
        _ => text,
    };
    try_parse_color(&text).unwrap_or_else(|e| panic!("{key} {field} {text}: {e}"))
}

fn labels(dark: bool) -> Vec<String> {
    ThemeId::of_appearance(dark)
        .into_iter()
        .map(|id| id.label().to_string())
        .collect()
}

// ---- catalogue

#[test]
fn dark_catalogue_matches_the_spec() {
    assert_eq!(labels(true), DARK);
}

#[test]
fn light_catalogue_matches_the_spec() {
    assert_eq!(labels(false), LIGHT);
}

#[test]
fn keys_are_unique_and_kebab_case() {
    let mut keys: Vec<String> = [true, false]
        .into_iter()
        .flat_map(ThemeId::of_appearance)
        .map(|id| id.key().to_string())
        .collect();
    assert_eq!(keys.len(), 44);
    for key in &keys {
        assert!(
            !key.is_empty()
                && !key.starts_with('-')
                && !key.ends_with('-')
                && !key.contains("--")
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{key}"
        );
    }
    assert_eq!(theme("qmt-dark"), ThemeId::QMT_DARK);
    assert_eq!(theme("macos-classic-dark").label(), "macOS Classic Dark");
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 44);
}

#[test]
fn resolve_falls_back_to_the_built_in_theme() {
    for saved in ["", "no-such-theme", "solarized-light", "qmt-light"] {
        assert_eq!(ThemeId::resolve(saved, true), ThemeId::QMT_DARK, "{saved}");
    }
    for saved in ["", "no-such-theme", "tokyo-night", "qmt-dark"] {
        assert_eq!(
            ThemeId::resolve(saved, false),
            ThemeId::QMT_LIGHT,
            "{saved}"
        );
    }
    let tokyo = ThemeId::resolve("tokyo-night", true);
    assert_eq!(tokyo.label(), "Tokyo Night");
    assert!(tokyo.dark());
    assert_eq!(
        ThemeId::resolve("solarized-light", false).label(),
        "Solarized Light"
    );
}

#[test]
fn bundled_configs_carry_no_highlight_block() {
    let files = parse_files();
    assert_eq!(files.len(), 42);
    assert_eq!(bundled().len(), 42);
    for id in bundled() {
        let config = id.config().expect("bundled");
        assert!(config.highlight.is_none(), "{}", id.label());
        let file = files
            .iter()
            .find(|c| c.name == id.label())
            .expect("file entry");
        let style = file.highlight.as_ref().expect("file has a highlight block");
        assert_eq!(&id.syntax().style, style, "{}", id.label());
    }
}

#[test]
fn every_bundled_theme_resolves_its_own_colours() {
    for id in bundled() {
        let key = id.key();
        let mut kit = Theme::default();
        kit.apply_config(&id.config().expect("bundled"));
        assert_close(
            kit.background,
            raw_color(&key, "background"),
            &format!("{key} background"),
        );
        assert_close(
            kit.foreground,
            raw_color(&key, "foreground"),
            &format!("{key} foreground"),
        );
        assert_close(
            kit.primary,
            raw_color(&key, "primary.background"),
            &format!("{key} primary"),
        );
    }
}

#[test]
fn every_bundled_theme_has_syntax_colours() {
    for id in bundled() {
        let syntax = id.syntax();
        for name in ["comment", "keyword", "string", "function", "type"] {
            assert!(
                syntax.style(name).and_then(|s| s.color).is_some(),
                "{} has no {name} colour",
                id.label()
            );
        }
    }
}

// ---- palette

#[test]
fn page_and_text_come_from_the_theme() {
    let p = theme("catppuccin-mocha").palette();
    assert_close(p.bg, raw_color("catppuccin-mocha", "background"), "bg");
    assert_close(p.text, raw_color("catppuccin-mocha", "foreground"), "text");
    assert_close(
        p.dim,
        raw_color("catppuccin-mocha", "muted.foreground"),
        "dim",
    );
    let (t, d, m) = (Rgba::from(p.text), Rgba::from(p.dim), Rgba::from(p.muted));
    for (t, d, m) in [(t.r, d.r, m.r), (t.g, d.g, m.g), (t.b, d.b, m.b)] {
        assert!(m >= t.min(d) - 1e-4 && m <= t.max(d) + 1e-4);
    }
}

#[test]
fn status_colours_come_from_base_colours() {
    let p = theme("gruvbox-dark").palette();
    assert_close(p.danger, raw_color("gruvbox-dark", "base.red"), "danger");
    assert_close(p.warn, raw_color("gruvbox-dark", "base.yellow"), "warn");
    assert_close(
        p.success,
        raw_color("gruvbox-dark", "base.green"),
        "success",
    );
}

#[test]
fn surface_tokens_are_opaque() {
    for id in bundled() {
        let p = id.palette();
        for (name, c) in [
            ("bg", p.bg),
            ("surface", p.surface),
            ("popover", p.popover),
            ("card", p.card),
            ("card_hover", p.card_hover),
            ("border", p.border),
            ("border_strong", p.border_strong),
        ] {
            assert!(
                (c.a - 1.).abs() < 1e-6,
                "{} {name} alpha {}",
                id.label(),
                c.a
            );
        }
    }
}

#[test]
fn comment_colour_is_the_themes_own() {
    let id = theme("catppuccin-mocha");
    let syntax = id.syntax();
    let spans = crate::highlight::highlight_code("rs", "// c", syntax.as_ref());
    let comment = spans[0].1.color.expect("comment colour");
    let file = raw("catppuccin-mocha");
    let want = try_parse_color(
        file["highlight"]["syntax"]["comment"]["color"]
            .as_str()
            .expect("comment colour in the file"),
    )
    .expect("parses");
    assert_close(comment, want, "comment");
}

/// What a converted theme must show: values of the theme of the same name in
/// Zed, copied from `assets/themes/{ayu,gruvbox,one}` at commit
/// `150c2aaeca8778c2759bba92b19247b0901d536f` (design D11).
struct Zed {
    page: u32,
    sidebar: u32,
    popover: u32,
    card: u32,
    selected: u32,
    border: u32,
    text: u32,
    dim: u32,
    accent: u32,
    comment: u32,
    keyword: u32,
    string: u32,
}

#[rustfmt::skip]
const ZED: [(&str, Zed); 6] = [
    ("ayu-mirage", Zed { page: 0x242835, sidebar: 0x353944, popover: 0x353944, card: 0x353944, selected: 0x53565d, border: 0x53565d, text: 0xcccac2, dim: 0x9a9a98, accent: 0x72cffe, comment: 0x5c6773, keyword: 0xffad65, string: 0xd4fe7f }),
    ("gruvbox-dark-soft", Zed { page: 0x32302f, sidebar: 0x3b3735, popover: 0x3b3735, card: 0x3b3735, selected: 0x5b524c, border: 0x5b534d, text: 0xfbf1c7, dim: 0xc5b597, accent: 0x83a598, comment: 0xa89984, keyword: 0xfb4833, string: 0xb8bb25 }),
    ("gruvbox-light-hard", Zed { page: 0xf9f5d7, sidebar: 0xecddb5, popover: 0xecddb5, card: 0xecddb5, selected: 0xc8b899, border: 0xc8b899, text: 0x282828, dim: 0x5f5650, accent: 0x0b6678, comment: 0x7c6f64, keyword: 0x9d0006, string: 0x79740e }),
    ("gruvbox-light-soft", Zed { page: 0xf2e5bc, sidebar: 0xecdcb3, popover: 0xecdcb3, card: 0xecdcb3, selected: 0xc8b899, border: 0xc8b899, text: 0x282828, dim: 0x5f5650, accent: 0x0b6678, comment: 0x7c6f64, keyword: 0x9d0006, string: 0x79740e }),
    ("one-dark", Zed { page: 0x282c33, sidebar: 0x2f343e, popover: 0x2f343e, card: 0x2e343e, selected: 0x454a56, border: 0x464b57, text: 0xdce0e5, dim: 0xa9afbc, accent: 0x74ade8, comment: 0x5d636f, keyword: 0xb477cf, string: 0xa1c181 }),
    ("one-light", Zed { page: 0xfafafa, sidebar: 0xebebec, popover: 0xebebec, card: 0xebebec, selected: 0xcacaca, border: 0xc9c9ca, text: 0x242529, dim: 0x58585a, accent: 0x5c78e2, comment: 0xa2a3a7, keyword: 0xa449ab, string: 0x649f57 }),
];

#[test]
fn added_themes_follow_their_zed_originals() {
    for (key, zed) in ZED {
        let id = theme(key);
        let p = id.palette();
        let what = |part: &str| format!("{key} {part}");
        assert_close(p.bg, hex(zed.page), &what("page"));
        assert_close(p.surface, hex(zed.sidebar), &what("sidebar"));
        assert_close(p.popover, hex(zed.popover), &what("popover"));
        assert_close(p.card, hex(zed.card), &what("card"));
        assert_close(p.selected, hex(zed.selected), &what("highlighted row"));
        assert_close(p.border, hex(zed.border), &what("border"));
        assert_close(p.text, hex(zed.text), &what("text"));
        assert_close(p.dim, hex(zed.dim), &what("faint text"));
        assert_close(p.accent, hex(zed.accent), &what("accent"));
        let syntax = id.syntax();
        let colour = |style: &Option<_>| -> Hsla {
            let style: &gpui_kit::component::highlighter::ThemeStyle =
                style.as_ref().expect("style");
            style.color.expect("colour")
        };
        assert_close(
            colour(&syntax.style.syntax.comment),
            hex(zed.comment),
            &what("comment"),
        );
        assert_close(
            colour(&syntax.style.syntax.keyword),
            hex(zed.keyword),
            &what("keyword"),
        );
        assert_close(
            colour(&syntax.style.syntax.string),
            hex(zed.string),
            &what("string"),
        );
    }
    // The spec's two examples, by value.
    assert_close(
        theme("one-dark").palette().bg,
        hex(0x282c33),
        "One Dark page",
    );
    assert_close(
        theme("gruvbox-light-soft").palette().bg,
        hex(0xf2e5bc),
        "Gruvbox Light Soft page",
    );
    assert!(theme("one-dark").dark() && !theme("one-light").dark());
}

#[test]
fn built_in_palettes_are_the_existing_ones() {
    let (dark, light) = (ThemeId::QMT_DARK, ThemeId::QMT_LIGHT);
    assert_eq!(dark.palette(), Palette::dark());
    assert_eq!(light.palette(), Palette::light());
    assert_eq!(dark.syntax(), HighlightTheme::default_dark());
    assert_eq!(light.syntax(), HighlightTheme::default_light());
    assert_eq!(dark.key(), "qmt-dark");
    assert_eq!(light.label(), "QueryMT Light");
}

#[test]
fn syntax_lookup_is_cached() {
    let p = theme("tokyo-night").palette();
    assert!(Arc::ptr_eq(&syntax(&p), &syntax(&p)));
    assert!(!Arc::ptr_eq(&syntax(&p), &syntax(&Palette::dark())));
}

// ---- accent

#[test]
fn contrast_of_black_on_white_is_21() {
    assert!((contrast(hex(0x000000), hex(0xffffff)) - 21.).abs() < 1e-3);
    assert!((contrast(hex(0x777777), hex(0x777777)) - 1.).abs() < 1e-6);
}

#[test]
fn accent_parse_reads_ids() {
    assert_eq!(Accent::parse(""), Accent::Theme);
    assert_eq!(Accent::parse("chartreuse"), Accent::Theme);
    assert_eq!(Accent::Theme.id(), "");
    for preset in Preset::all() {
        let accent = Accent::Preset(preset);
        assert_eq!(Accent::parse(accent.id()), accent);
    }
    assert_eq!(Preset::all().count(), 9);
}

fn preset(id: &str) -> Preset {
    Preset::all().find(|p| p.id() == id).expect("preset")
}

fn shown(key: &str, accent: Accent) -> Palette {
    Appearance {
        theme: theme(key),
        accent,
    }
    .palette()
}

#[test]
fn theme_accent_by_default() {
    let p = shown("tokyo-night", Accent::Theme);
    assert_close(
        p.accent,
        raw_color("tokyo-night", "primary.background"),
        "accent",
    );
    assert_close(
        p.on_accent,
        raw_color("tokyo-night", "primary.foreground"),
        "on_accent",
    );
}

#[test]
fn preset_overrides_the_theme() {
    let base = theme("gruvbox-dark").palette();
    let p = shown("gruvbox-dark", Accent::Preset(preset("blue")));
    assert_close(p.accent, hex(0x60a5fa), "accent");
    assert_close(
        p.on_accent,
        base.on_accent,
        "on_accent is the theme's own label",
    );
    assert!(contrast(p.on_accent, p.accent) > MIN_LABEL_CONTRAST);
}

#[test]
fn label_below_3_to_1_becomes_black_or_white() {
    let hybrid = theme("hybrid-dark").palette();
    assert!(contrast(hybrid.on_accent, hex(0xfacc15)) < MIN_LABEL_CONTRAST);
    let p = shown("hybrid-dark", Accent::Preset(preset("amber")));
    assert_close(p.on_accent, hex(0x000000), "black on amber");
    // A dark label on a light fill keeps the theme's own.
    let tokyo = theme("tokyo-night").palette();
    assert!(contrast(tokyo.on_accent, hex(0xfacc15)) >= MIN_LABEL_CONTRAST);
    let p = shown("tokyo-night", Accent::Preset(preset("amber")));
    assert_close(p.on_accent, tokyo.on_accent, "own label kept");
}

#[test]
fn orange_preset_overrides_a_bundled_theme() {
    let tokyo = theme("tokyo-night").palette();
    let p = shown("tokyo-night", Accent::Preset(Preset::ORANGE));
    assert_close(p.accent, hex(0xfb923c), "orange fill");
    assert!(!close(p.accent, tokyo.accent));
}

#[test]
fn own_accent_preset_changes_nothing() {
    for id in [ThemeId::QMT_DARK, ThemeId::QMT_LIGHT] {
        let orange = Appearance {
            theme: id,
            accent: Accent::Preset(Preset::ORANGE),
        }
        .palette();
        let own = Appearance {
            theme: id,
            accent: Accent::Theme,
        }
        .palette();
        assert_eq!(orange, own);
    }
}

#[test]
fn preset_changes_only_the_accent_fields() {
    let sample = [
        ThemeId::QMT_DARK,
        ThemeId::QMT_LIGHT,
        theme("tokyo-night"),
        theme("solarized-light"),
        theme("hybrid-dark"),
        theme("aurora-light"),
    ];
    for id in sample {
        let base = id.palette();
        for preset in Preset::all() {
            if id.own_accent() == Some(preset) {
                continue;
            }
            let p = Appearance {
                theme: id,
                accent: Accent::Preset(preset),
            }
            .palette();
            let fill = preset.color(base.dark);
            let step = if base.dark { 0.08 } else { -0.08 };
            assert_close(p.accent, fill, "accent");
            assert_close(
                p.accent_hover,
                Hsla {
                    l: (fill.l + step).clamp(0., 1.),
                    ..fill
                },
                "accent_hover",
            );
            assert_close(
                p.accent_dim,
                fill.opacity(if base.dark { 0.16 } else { 0.12 }),
                "accent_dim",
            );
            let rest = Palette {
                accent: base.accent,
                accent_hover: base.accent_hover,
                accent_dim: base.accent_dim,
                on_accent: base.on_accent,
                ..p
            };
            assert_eq!(rest, base, "{} {}", id.label(), preset.id());
        }
    }
}

#[test]
fn built_in_dark_labels_are_unchanged() {
    for preset in Preset::all() {
        let p = Appearance {
            theme: ThemeId::QMT_DARK,
            accent: Accent::Preset(preset),
        }
        .palette();
        assert_close(p.on_accent, hex(0x1b1b1b), preset.id());
    }
}

#[test]
fn built_in_light_labels_follow_3_to_1() {
    for (id, label) in [
        ("teal", 0x000000),
        ("green", 0x000000),
        ("amber", 0x000000),
        ("blue", 0xffffff),
        ("red", 0xffffff),
        ("pink", 0xffffff),
        ("purple", 0xffffff),
        ("slate", 0xffffff),
    ] {
        let p = Appearance {
            theme: ThemeId::QMT_LIGHT,
            accent: Accent::Preset(preset(id)),
        }
        .palette();
        assert_close(p.on_accent, hex(label), id);
    }
}

#[test]
fn orange_is_the_built_in_accent() {
    assert_close(Preset::ORANGE.color(true), Palette::dark().accent, "dark");
    assert_close(
        Preset::ORANGE.color(false),
        Palette::light().accent,
        "light",
    );
    assert_close(
        Preset::ORANGE.dim(true),
        Palette::dark().accent_dim,
        "dark tint",
    );
    assert_close(
        Preset::ORANGE.dim(false),
        Palette::light().accent_dim,
        "light tint",
    );
}

#[test]
fn theme_swatch_colour_is_the_themes_own() {
    assert_close(
        theme("tokyo-night").accent(),
        raw_color("tokyo-night", "primary.background"),
        "swatch",
    );
}

// ---- apply

fn start(cx: &mut TestAppContext, appearance: Appearance) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        apply(appearance, cx);
    });
}

fn show(cx: &mut TestAppContext, key: &str) {
    let id = theme(key);
    cx.update(|cx| {
        apply(
            Appearance {
                theme: id,
                accent: Accent::Theme,
            },
            cx,
        )
    });
}

fn fresh(cx: &mut TestAppContext, key: &str) -> Appearance {
    let appearance = Appearance {
        theme: theme(key),
        accent: Accent::Theme,
    };
    start(cx, appearance);
    appearance
}

fn kit_json(cx: &mut TestAppContext) -> Value {
    cx.update(|cx| serde_json::to_value(Theme::global(cx)).expect("theme serialises"))
}

fn kit_colors(cx: &mut TestAppContext) -> Value {
    cx.update(|cx| serde_json::to_value(Theme::global(cx).colors).expect("colours serialise"))
}

#[gpui_kit::test]
fn switching_modes_keeps_the_palette(
    cx: &mut TestAppContext,
    night: &mut TestAppContext,
    solar: &mut TestAppContext,
) {
    fresh(night, "tokyo-night");
    fresh(solar, "solarized-light");
    let (night, solar) = (kit_colors(night), kit_colors(solar));
    fresh(cx, "tokyo-night");
    assert_eq!(kit_colors(cx), night);
    show(cx, "solarized-light");
    assert_eq!(kit_colors(cx), solar);
    show(cx, "tokyo-night");
    assert_eq!(kit_colors(cx), night);
}

#[test]
fn built_in_highlight_is_the_card_colour() {
    for id in [ThemeId::QMT_DARK, ThemeId::QMT_LIGHT] {
        let p = id.palette();
        assert_eq!(p.selected, p.card, "{}", id.label());
    }
}

#[test]
fn bundled_highlight_is_the_themes_own_and_shows() {
    for id in bundled() {
        let key = id.key();
        let p = id.palette();
        // The file's `accent.background` (the converted themes set it from
        // Zed's selected element), composited over the page.
        let file = raw_color(&key, "accent.background");
        assert_close(
            p.selected,
            Hsla { a: 1., ..file },
            &format!("{key} highlighted row"),
        );
        // It must not equal what the menu and palette are painted on.
        assert!(!close(p.selected, p.surface), "{key}: equals the sidebar");
        assert!(!close(p.selected, p.popover), "{key}: equals the popover");
    }
}

#[gpui_kit::test]
fn kit_accent_is_the_highlight_colour(cx: &mut TestAppContext) {
    start(cx, Appearance::builtin(true));
    for key in [
        "one-dark",
        "ayu-mirage",
        "qmt-dark",
        "solarized-light",
        "qmt-light",
    ] {
        show(cx, key);
        cx.update(|cx| {
            let p = *palette(cx);
            assert_eq!(Theme::global(cx).accent, p.selected, "{key}");
            assert_eq!(Theme::global(cx).sidebar_accent, p.selected, "{key}");
        });
    }
    // The case that was invisible: One Dark's highlight differs from its panel.
    show(cx, "one-dark");
    cx.update(|cx| {
        assert_close(
            Theme::global(cx).accent,
            hex(0x454a56),
            "One Dark highlight",
        );
        assert!(!close(Theme::global(cx).accent, palette(cx).surface));
    });
}

#[gpui_kit::test]
fn kit_popover_comes_from_the_palette(cx: &mut TestAppContext) {
    fresh(cx, "tokyo-night");
    cx.update(|cx| {
        assert_eq!(Theme::global(cx).popover, palette(cx).popover);
        assert_close(
            palette(cx).popover,
            theme("tokyo-night").palette().popover,
            "popover",
        );
    });
}

#[gpui_kit::test]
fn kit_holds_the_entrys_highlight_theme(cx: &mut TestAppContext) {
    start(cx, Appearance::builtin(true));
    for key in ["tokyo-night", "solarized-light", "qmt-dark"] {
        let id = theme(key);
        cx.update(|cx| {
            apply(
                Appearance {
                    theme: id,
                    accent: Accent::Theme,
                },
                cx,
            )
        });
        cx.update(|cx| {
            assert!(
                Arc::ptr_eq(&Theme::global(cx).highlight_theme, &syntax(palette(cx))),
                "{key}"
            );
            assert!(
                Arc::ptr_eq(&Theme::global(cx).highlight_theme, &id.syntax()),
                "{key}"
            );
        });
    }
}

#[gpui_kit::test]
fn theme_fonts_are_ignored(cx: &mut TestAppContext) {
    let defaults = Theme::default();
    start(cx, Appearance::builtin(true));
    for id in bundled() {
        cx.update(|cx| {
            apply(
                Appearance {
                    theme: id,
                    accent: Accent::Theme,
                },
                cx,
            )
        });
        cx.update(|cx| {
            let t = Theme::global(cx);
            let what = id.label();
            assert_eq!(t.font_family, "IBM Plex Sans", "{what}");
            assert_eq!(t.mono_font_family, "IBM Plex Mono", "{what}");
            assert_eq!(t.radius, px(8.), "{what}");
            assert_eq!(t.radius_lg, px(12.), "{what}");
            assert_eq!(t.font_size, defaults.font_size, "{what}");
            assert_eq!(t.mono_font_size, defaults.mono_font_size, "{what}");
            assert!(t.shadow, "{what}");
        });
    }
    // The load step clears them from any config, whatever it sets.
    let config: ThemeConfig = serde_json::from_value(serde_json::json!({
        "name": "Loud", "mode": "dark",
        "font.size": 31, "font.family": "Comic Sans", "mono_font.family": "Courier",
        "mono_font.size": 29, "radius": 40, "radius.lg": 41, "window.radius": 42,
        "shadow": false,
    }))
    .expect("config");
    let (config, _) = prepare(config);
    let mut kit = Theme::default();
    kit.apply_config(&config);
    assert_eq!(kit.font_size, defaults.font_size);
    assert_eq!(kit.font_family, defaults.font_family);
    assert_eq!(kit.mono_font_family, defaults.mono_font_family);
    assert_eq!(kit.mono_font_size, defaults.mono_font_size);
    assert_eq!(kit.radius, defaults.radius);
    assert_eq!(kit.radius_lg, defaults.radius_lg);
    assert_eq!(kit.window_radius, defaults.window_radius);
    assert!(kit.shadow);
}

#[gpui_kit::test]
fn built_in_after_bundled_leaves_nothing_behind(
    cx: &mut TestAppContext,
    fresh_dark: &mut TestAppContext,
    light: &mut TestAppContext,
    fresh_light: &mut TestAppContext,
) {
    start(fresh_dark, Appearance::builtin(true));
    start(fresh_light, Appearance::builtin(false));
    start(cx, Appearance::builtin(true));
    show(cx, "tokyo-night");
    assert_ne!(kit_json(cx), kit_json(fresh_dark));
    show(cx, "qmt-dark");
    assert_eq!(kit_json(cx), kit_json(fresh_dark));
    cx.update(|cx| {
        assert!(Arc::ptr_eq(
            &Theme::global(cx).highlight_theme,
            &HighlightTheme::default_dark()
        ));
    });

    start(light, Appearance::builtin(false));
    show(light, "solarized-light");
    assert_ne!(kit_json(light), kit_json(fresh_light));
    show(light, "qmt-light");
    assert_eq!(kit_json(light), kit_json(fresh_light));
}
