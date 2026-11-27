#!/usr/bin/env python3
"""Converts six of Zed's built-in themes into gpui-kit theme files.

Not part of the build: the three JSON files next to this script are committed,
and this script is how they were made (openspec change `desktop-themes`,
design D11). Run it from this directory to reproduce them:

    python3 convert_zed.py

It fetches Zed's files at the commit named in ZED_COMMIT, so the output does
not depend on Zed's main branch.
"""
import colorsys
import json
import urllib.request

ZED_COMMIT = "150c2aaeca8778c2759bba92b19247b0901d536f"
RAW = "https://raw.githubusercontent.com/zed-industries/zed/%s/assets/themes/%s"
BLOB = "https://github.com/zed-industries/zed/blob/%s/assets/themes/%s"
SCHEMA = "https://github.com/longbridge/gpui-kit/raw/refs/heads/main/.theme-schema.json"

# (output file, set name, Zed file, themes to take, upstream project)
SETS = [
    ("ayu-mirage.json", "Ayu Mirage", "ayu/ayu.json", ["Ayu Mirage"], "dempfi/ayu"),
    (
        "gruvbox-variants.json",
        "Gruvbox Variants",
        "gruvbox/gruvbox.json",
        ["Gruvbox Dark Soft", "Gruvbox Light Hard", "Gruvbox Light Soft"],
        "morhetz/gruvbox",
    ),
    ("one.json", "One", "one/one.json", ["One Dark", "One Light"], "atom/atom one-dark-ui, one-light-ui"),
]

# The kit's `StatusColors` and `SyntaxColors` keys (gpui-component 0.11,
# `highlighter/registry.rs`); `comment.doc` is the spelling both Zed and the
# kit's own files use.
STATUS = [
    f"{name}{suffix}"
    for name in ("error", "warning", "info", "success", "hint")
    for suffix in ("", ".background", ".border")
] + ["created", "modified", "deleted"]
SYNTAX = {
    "attribute", "boolean", "comment", "comment.doc", "constant", "constructor",
    "embedded", "emphasis", "emphasis.strong", "enum", "function", "hint",
    "keyword", "label", "link_text", "link_uri", "number", "operator",
    "predictive", "preproc", "primary", "property", "punctuation",
    "punctuation.bracket", "punctuation.delimiter", "punctuation.list_marker",
    "punctuation.special", "string", "string.escape", "string.regex",
    "string.special", "string.special.symbol", "tag", "tag.doctype",
    "text.code.span", "text.literal", "title", "type", "variable",
    "variable.special", "variant",
}
EDITOR = [
    "editor.background", "editor.foreground", "editor.active_line.background",
    "editor.line_number", "editor.active_line_number", "editor.invisible",
]


def rgb(hex_colour):
    h = hex_colour.lstrip("#")
    return tuple(int(h[i : i + 2], 16) / 255 for i in (0, 2, 4))


def luminance(hex_colour):
    def lin(c):
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4

    r, g, b = (lin(c) for c in rgb(hex_colour))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(a, b):
    la, lb = sorted((luminance(a), luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


def short(hex_colour):
    """`#rrggbbff` -> `#rrggbb`; translucent values keep their alpha."""
    c = hex_colour.lower()
    return c[:7] if len(c) == 9 and c.endswith("ff") else c


def hover(hex_colour, dark):
    """The accent's lightness step (design D6): +0.08 dark, -0.08 light."""
    h, l, s = colorsys.rgb_to_hls(*rgb(hex_colour))
    l = min(1.0, max(0.0, l + (0.08 if dark else -0.08)))
    r, g, b = colorsys.hls_to_rgb(h, l, s)
    return "#%02x%02x%02x" % tuple(round(c * 255) for c in (r, g, b))


def convert(theme):
    s = theme["style"]
    dark = theme["appearance"] == "dark"
    accent = short(s["text.accent"])
    page = short(s["editor.background"])
    text = short(s["text"])
    label = page if contrast(accent, page) >= contrast(accent, text) else text
    colors = {
        "background": page,
        "foreground": text,
        "border": short(s["border"]),
        "input.border": short(s["border.variant"]),
        "muted.foreground": short(s["text.muted"]),
        "sidebar.background": short(s["surface.background"]),
        "popover.background": short(s["elevated_surface.background"]),
        "popover.foreground": text,
        "secondary.background": short(s["element.background"]),
        "secondary.hover.background": short(s["element.hover"]),
        "secondary.foreground": text,
        # The highlighted row. Zed's element, surface and elevated surface are
        # one colour, so without this the row would equal its panel.
        "accent.background": short(s["element.selected"]),
        "accent.foreground": text,
        "primary.background": accent,
        "primary.hover.background": hover(accent, dark),
        "primary.foreground": label,
        "base.red": short(s["error"]),
        "base.yellow": short(s["warning"]),
        "base.green": short(s["success"]),
    }
    highlight = {k: short(s[k]) for k in EDITOR if s.get(k)}
    highlight.update({k: short(s[k]) for k in STATUS if s.get(k)})
    syntax = {}
    for key, style in s["syntax"].items():
        if key not in SYNTAX:
            continue
        entry = {k: (short(v) if k == "color" else v) for k, v in style.items()
                 if k in ("color", "font_style", "font_weight") and v is not None}
        if entry:
            syntax[key] = entry
    highlight["syntax"] = syntax
    return {
        "name": theme["name"],
        "mode": theme["appearance"],
        "colors": colors,
        "highlight": highlight,
    }


def main():
    for out, name, source, wanted, project in SETS:
        doc = json.load(urllib.request.urlopen(RAW % (ZED_COMMIT, source)))
        themes = {t["name"]: t for t in doc["themes"]}
        result = {
            "$schema": SCHEMA,
            "name": name,
            "author": "Zed Industries (%s)" % project,
            "url": BLOB % (ZED_COMMIT, source.rsplit("/", 1)[0]),
            "themes": [convert(themes[n]) for n in wanted],
        }
        with open(out, "w") as f:
            json.dump(result, f, indent=2, ensure_ascii=False)
            f.write("\n")
        print("wrote", out, [t["name"] for t in result["themes"]])


if __name__ == "__main__":
    main()
