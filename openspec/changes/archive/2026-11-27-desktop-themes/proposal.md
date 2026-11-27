# Proposal

## Why

QueryMT Desktop has one dark and one light look, plus an accent colour. Users
who find the dark theme hard on the eyes, or who use Catppuccin or Tokyo
Night everywhere else, have no way to change it. A first attempt
(`dark-theme-legibility`) re-tuned the built-in palettes to pass contrast
numbers, and the result looked worse; it is dropped. Letting users pick a
designed, whole-app theme is the better answer. The component kit the app is
built on (gpui-kit) already defines a theme file format, parses it on native
and web, and its repository ships 36 such themes with app colours and syntax
colours chosen by their authors.

## What Changes

- Settings -> Appearance gets a **Dark theme** and a **Light theme** select
  next to the existing System / Light / Dark cards. The mode cards keep
  deciding which of the two is shown; "System" switches between them with the
  desktop appearance.
- The built-in looks become two named themes, **QueryMT Dark** and **QueryMT
  Light**, and stay the defaults. Their colours, syntax colours and sizes do
  not change.
- The 36 themes from the gpui-kit repository's `themes/` folder (25 dark,
  11 light: Catppuccin, Tokyo Night, Ayu, Everforest, Flexoki, Gruvbox,
  Solarized, macOS Classic, Molokai, Mellifluous, Hybrid and others) are
  bundled with the app and offered in the list of their appearance.
- Six more themes complete the set Zed ships built in: **Ayu Mirage**,
  **Gruvbox Dark Soft**, **One Dark** (dark), **Gruvbox Light Hard**,
  **Gruvbox Light Soft** and **One Light** (light). gpui-kit has none of
  them, so they are converted once, by hand, from Zed's theme files into the
  kit's format and bundled next to the kit's files. That makes 42 bundled
  themes, 28 dark and 14 light. Zed's Gruvbox Dark Hard is left out: the
  kit's Gruvbox Dark already uses gruvbox's hard background (`#1d2021`).
- The command palette (Ctrl/Cmd+P) gets **Select theme…**, which lists
  every theme, dark and light, with the one shown marked. Moving through the
  list previews the highlighted theme in the window; Enter keeps it, Esc or a
  click outside puts the previous look back. Picking a theme of the
  appearance shown keeps the mode (System stays System); picking one of the
  other appearance also switches the mode to Dark or Light.
- Choosing a bundled theme recolours the whole window from the theme's own
  colours: page, sidebar, popovers, cards, borders, text, secondary text,
  status colours, and code in fences, tool output, diffs and inputs. Themes
  are shown as their authors made them; the only contrast rule is for the
  button label on a picked accent preset (next item).
- The accent follows the theme by default: the Accent color row gains a
  **Theme** swatch, selected unless the user picked a preset. A picked
  preset overrides the accent on every theme. The label on a button with a
  picked preset keeps the theme's own label colour when that has at least
  3:1 contrast with the preset's fill, and is black or white otherwise.
  QueryMT Dark and Light keep their orange accent and look as they do
  today, with one deliberate difference: on QueryMT Light the label on the
  Teal, Green and Amber presets turns from white (1.9 to 2.5:1) to black.
  An `orange` saved by an older version (it was the default, not a choice)
  is read as "the theme's accent" once, when the file is first loaded by
  this version.
- The settings preview cards show the selected themes.
- The quick panel does not follow the app theme. It keeps its dark glass
  palette (taking only the accent, as now) and always uses QueryMT Dark's
  syntax colours; today it picks up the app's syntax colours, which in light
  mode puts light-theme code colours on the dark panel.
- Two new saved preferences hold the chosen dark and light theme, and an
  internal third one versions the appearance preferences (it is what tells
  an older file from a newer one). An unknown or removed theme id falls back
  to the QueryMT theme of that appearance, and the select marks the theme
  actually shown.

## Non-goals

- Changing the QueryMT Dark or QueryMT Light colours, text sizes or font
  sizes, or adding contrast floors and tests for a theme's own colours (the
  dropped `dark-theme-legibility` work). The label rule above applies only
  to a picked accent preset.
- Fonts, font sizes and corner radii from theme files; the app keeps its
  own.
- Loading user theme files from disk, Zed or VS Code theme import, or the
  editor (tmTheme) themes bundled in `two-face`. The six Zed themes above are
  converted once and committed; the app has no importer.
- A keyboard shortcut for the theme picker (Zed's Cmd+K Cmd+T: Cmd+K belongs
  to the quick panel); it is reached through the command palette.
- A separate link colour per accent, or new accent presets.
- Per-window or per-session themes.
- Theming the quick panel beyond its accent, the app icon or the tray.
- Syncing the theme with the querymt-desktop (Svelte) app or the agent.

## Capabilities

### New Capabilities

- `desktop-themes`: which colour themes QueryMT Desktop offers, how the
  user picks the dark and light theme and the accent, what a theme
  recolours, how the choice is saved and restored, and what stays
  independent of it (the quick panel).

### Modified Capabilities

None. `mermaid-diagrams` already requires diagrams to use the current
theme's colours; deriving the palette from the chosen theme satisfies it
without a requirement change.

## Impact

- Package: `querymt-gpui` only, default features (native and
  `wasm32-unknown-unknown`). No other package depends on it.
- Code: `src/theme.rs` grows into a `src/theme/` module (theme catalogue,
  conversion of a kit theme into the app palette and syntax colours, the
  `ThemeId`, `Accent` and `Appearance` types, `apply`), `src/store.rs`
  (`Prefs` and its migration in `startup_state`),
  `src/ui/settings.rs` (Appearance selects, Theme accent swatch, previews),
  `src/ui/mod.rs` and `src/main.rs` (apply on start and on change),
  `src/ui/keys.rs` (the palette's theme picker),
  `src/ui/md.rs`, `src/ui/session_view.rs`, `src/ui/kit.rs` (syntax colours
  taken from the palette in use, so the quick panel can keep its own),
  `src/highlight.rs` (a test that fences and tool output agree; fences
  take the palette's syntax colours through the text view's style),
  `src/quick/hud.rs`, `src/film/scene.rs` and `src/film/mod.rs` (scenes may
  name a theme), and test call sites of `theme::apply`.
- Assets: 21 theme files (172,276 bytes of JSON, about 18 KB gzipped)
  copied from the gpui-kit repository at a pinned commit into
  `assets/themes/kit/`, with a notice naming the source, the licence and each
  theme's author. Three more files (Ayu Mirage; Gruvbox Dark Soft, Light
  Hard and Light Soft; One Dark and Light) converted from Zed's theme files
  into `assets/themes/extra/`, with a notice naming Zed's files, the
  upstream projects and their MIT licences. The web build's size before and
  after is measured.
- Dependencies: none added; the kit's own theme types parse the files.
- Persisted state: the desktop preferences file gains three optional keys
  (`theme_dark`, `theme_light`, `appearance_version`), and the existing
  `accent` key's empty value now means "the theme's accent" (it meant
  orange, which is still QueryMT's accent). A file without
  `appearance_version` (saved before themes existed) that holds `accent =
  "orange"` loads with an empty accent, the same look. Otherwise older
  files load unchanged; older app versions ignore the new keys.
- No public Rust API, agent config, ACP, typeshare, session storage or
  plugin contract changes. No querymt-desktop follow-up is required.
- The dropped `dark-theme-legibility` change has an unmerged implementation
  in a qmt worktree; it is not used by this change.
