# Spec Delta

## Purpose

Lets QueryMT Desktop users choose the colour theme of the app, separately for
dark and light appearance, from the built-in QueryMT themes and a set of
bundled whole-app themes, choose whether the accent follows the theme, and
keeps those choices across restarts.

## ADDED Requirements

### Requirement: Dark theme catalogue
The dark list SHALL offer QueryMT Dark, then: Adventure, Adventure Time, Alduin, Asciinema, Ayu Dark, Catppuccin Frappe, Catppuccin Macchiato, Catppuccin Mocha, Everforest Dark, Fahrenheit, Flexoki Dark, Gruvbox Dark, Harper, Hybrid Dark, Jellybeans, Kibble, macOS Classic Dark, Mellifluous Dark, Molokai Dark, Solarized Dark, Spaceduck, Tokyo Moon, Tokyo Night, Tokyo Storm, Twilight.

#### Scenario: Dark list
- **WHEN** the user opens the Dark theme select in Settings -> Appearance
- **THEN** it lists QueryMT Dark first, then the 25 dark themes above by name, and no light theme

### Requirement: Light theme catalogue
The light list SHALL offer QueryMT Light, then: Aurora Light, Ayu Light, Catppuccin Latte, Everforest Light, Flexoki Light, Gruvbox Light, Hybrid Light, macOS Classic Light, Mellifluous Light, Molokai Light, Solarized Light.

#### Scenario: Light list
- **WHEN** the user opens the Light theme select
- **THEN** it lists QueryMT Light first, then the 11 light themes above by name, and no dark theme

### Requirement: A dark and a light theme are chosen separately
The user SHALL pick one dark theme and one light theme. The existing System / Light / Dark mode SHALL decide which is shown: Dark shows the dark theme, Light the light theme, System the one matching the desktop appearance, switching when the desktop switches. Picking a theme SHALL apply at once when it is the one shown.

#### Scenario: Pick a dark theme while in dark mode
- **GIVEN** the mode is Dark
- **WHEN** the user picks Tokyo Night in the Dark theme select
- **THEN** the window is recoloured with Tokyo Night without a restart

#### Scenario: Pick a light theme while in dark mode
- **GIVEN** the mode is Dark and the dark theme is Tokyo Night
- **WHEN** the user picks Solarized Light in the Light theme select
- **THEN** the window stays Tokyo Night, and Solarized Light is shown as the light theme

#### Scenario: System mode follows the desktop
- **GIVEN** the mode is System, the dark theme is Catppuccin Mocha and the light theme is Catppuccin Latte
- **WHEN** the desktop appearance changes from light to dark
- **THEN** the window changes from Catppuccin Latte to Catppuccin Mocha

#### Scenario: Previews show the chosen themes
- **GIVEN** the dark theme is Gruvbox Dark and the light theme is Flexoki Light
- **WHEN** the user views the System / Light / Dark cards
- **THEN** the Dark card previews Gruvbox Dark, the Light card Flexoki Light, and the System card one half of each

### Requirement: Built-in themes are unchanged
QueryMT Dark and QueryMT Light SHALL be the defaults, and SHALL show exactly the colours, syntax colours and text sizes the app showed before themes existed, with an orange accent. With an accent preset picked they SHALL show that preset's fill, hover colour and tint exactly as the app did before themes existed. Showing a bundled theme and then QueryMT Dark or Light again SHALL leave nothing of the bundled theme behind.

#### Scenario: Fresh install
- **GIVEN** no theme and no accent were ever picked
- **WHEN** the app starts in dark mode
- **THEN** it shows QueryMT Dark with the orange accent, identical to the previous dark look

#### Scenario: Preset on a built-in theme
- **GIVEN** the shown theme is QueryMT Light
- **WHEN** the user picks the Blue preset
- **THEN** primary buttons use Blue's light fill and its hover colour with a white label, as before themes existed

#### Scenario: Preset whose white label is too faint
- **GIVEN** the shown theme is QueryMT Light
- **WHEN** the user picks the Amber preset
- **THEN** primary buttons use Amber's light fill with a black label, because white has only 1.9:1 contrast with it

#### Scenario: Back from a bundled theme
- **GIVEN** the shown theme is Tokyo Night
- **WHEN** the user picks QueryMT Dark in the Dark theme select
- **THEN** every colour, including the syntax colours and the shadow setting, equals what a fresh start with QueryMT Dark shows

### Requirement: A bundled theme recolours the whole window
With a bundled theme shown, the page, sidebar, popovers, cards, hovered cards and borders SHALL use the theme's own colours for them. Main text SHALL be the theme's foreground and the faintest text its muted foreground. Errors, warnings and successes SHALL use the theme's red, yellow and green.

#### Scenario: Page and text
- **WHEN** the shown theme is Catppuccin Mocha
- **THEN** the page background is the theme's background colour, message text its foreground, and timestamps and other faint labels its muted foreground

#### Scenario: Status colours
- **GIVEN** the shown theme is Gruvbox Dark
- **WHEN** a failed tool call and a finished one are shown
- **THEN** the failure uses the theme's red and the success its green

#### Scenario: Fonts and sizes stay the app's
- **GIVEN** any bundled theme is shown
- **THEN** text uses the app's fonts, sizes and corner radii, the same as with QueryMT Dark

### Requirement: Code uses the theme's syntax colours
Fenced code in messages, tool output, diffs and code inputs in the main window SHALL use the shown theme's syntax colours; code kinds the theme gives no colour SHALL use its foreground. Themes SHALL be shown as authored: their colours SHALL NOT be adjusted for contrast.

#### Scenario: Comment colour
- **GIVEN** the shown theme is Catppuccin Mocha
- **WHEN** a Rust fence with a `// comment` is shown
- **THEN** the comment has the theme's comment colour, unchanged

#### Scenario: Tool output and fences agree
- **GIVEN** any bundled theme is shown
- **WHEN** the same Rust code appears in a message fence and in a file-read tool output
- **THEN** each token has the same colour in both

### Requirement: The accent follows the theme unless a preset is picked
The Accent color row SHALL offer a Theme swatch, showing the shown theme's accent, before the presets. With Theme selected, buttons, focus rings, the caret, selections, links and user-message tints SHALL use the theme's accent and button labels its label colour. A picked preset SHALL override the accent on every theme.

#### Scenario: Theme accent by default
- **GIVEN** no accent preset was ever picked and the shown theme is Tokyo Night
- **THEN** the Theme swatch is selected and primary buttons use Tokyo Night's accent colour and its button label colour

#### Scenario: Preset overrides the theme
- **GIVEN** the shown theme is Gruvbox Dark
- **WHEN** the user picks the Blue preset
- **THEN** primary buttons use the Blue preset's dark fill, with Gruvbox Dark's own label colour, which has more than 3:1 contrast with it

#### Scenario: Orange preset on a bundled theme
- **GIVEN** the shown theme is Tokyo Night
- **WHEN** the user picks the Orange preset
- **THEN** primary buttons use Orange's dark fill, not Tokyo Night's accent

#### Scenario: Back to the theme's accent
- **GIVEN** the Blue preset is picked
- **WHEN** the user picks the Theme swatch
- **THEN** the shown theme's own accent is used again

#### Scenario: Built-in theme accent
- **GIVEN** the Theme swatch is selected
- **WHEN** QueryMT Dark is shown
- **THEN** the accent is QueryMT's orange, as before

### Requirement: Preset labels stay legible
The button label colour with a preset SHALL be the shown theme's own label colour when it has at least 3:1 contrast with the preset's fill, and otherwise black or white, whichever contrasts more. A theme's own accent and label SHALL be shown as the theme defines them, whatever their contrast, and a preset equal to the theme's own accent SHALL change nothing. On QueryMT Dark nothing changes; on QueryMT Light only Teal, Green and Amber change (white label before, black now).

#### Scenario: Label colour below 3:1 on a bundled theme
- **GIVEN** the shown theme is Hybrid Dark, whose label colour is a light grey
- **WHEN** the user picks the Amber preset
- **THEN** primary button labels are black, because the theme's own label has only 1.3:1 contrast with Amber's fill

#### Scenario: The theme's own accent is not adjusted
- **GIVEN** the shown theme is QueryMT Light with the Theme swatch selected
- **THEN** primary buttons use its orange accent with its white label, as before themes existed

#### Scenario: Preset equal to the built-in accent
- **GIVEN** the shown theme is QueryMT Dark
- **WHEN** the user picks the Orange preset
- **THEN** every accent colour equals what the Theme swatch shows

### Requirement: The Theme swatch shows the theme's own accent
The Theme swatch SHALL show the shown theme's own accent even while a preset is picked, so it keeps showing what picking it would restore.

#### Scenario: Theme swatch while a preset is picked
- **GIVEN** the shown theme is Tokyo Night and the Blue preset is picked
- **THEN** the Theme swatch is painted with Tokyo Night's own accent, not Blue

### Requirement: The quick panel keeps its own colours
The quick panel SHALL not follow the app theme. Its surfaces, text and status colours SHALL stay its dark panel colours, and code shown in it SHALL use QueryMT Dark's syntax colours whatever theme or mode the main window shows. The accent in use in the main window SHALL still apply.

#### Scenario: Bundled theme in the main window
- **GIVEN** the main window shows Solarized Light
- **WHEN** the quick panel shows a session with a code fence and a diff
- **THEN** the panel's surfaces are its own dark colours and the code uses QueryMT Dark's syntax colours

#### Scenario: Light mode with the built-in theme
- **GIVEN** the main window shows QueryMT Light
- **WHEN** the quick panel shows a code fence
- **THEN** the code uses QueryMT Dark's syntax colours, not QueryMT Light's

### Requirement: The theme choice is saved
The chosen themes SHALL be saved with the other desktop preferences (`state.json` in the `querymt-gpui` config directory on desktop, the `querymt-gpui:state` local-storage entry on the web) as `prefs.theme_dark` and `prefs.theme_light`: string theme ids, default unset, meaning QueryMT Dark or Light. The existing `prefs.accent` unset or empty SHALL mean the theme's accent.

#### Scenario: Restored after restart
- **GIVEN** the user picked Tokyo Night as the dark theme
- **WHEN** the app restarts in dark mode
- **THEN** it shows Tokyo Night

#### Scenario: Older preferences
- **GIVEN** a preferences file saved before themes existed, with no accent picked
- **WHEN** the app starts
- **THEN** every other preference loads unchanged, the QueryMT themes are shown, and the accent is orange

#### Scenario: Unknown or wrong-appearance id
- **GIVEN** `prefs.theme_dark` names no bundled theme, or names a light theme
- **WHEN** the app starts in dark mode
- **THEN** it shows QueryMT Dark, reports no error, and keeps the saved value until the user picks another theme

#### Scenario: Value that is not a string
- **GIVEN** `prefs.theme_light` holds a number
- **WHEN** the app starts
- **THEN** the light theme is treated as unset, and every other preference and agent still loads

### Requirement: An orange saved before themes is the theme's accent
Preferences SHALL record an appearance version. A preferences file saved before themes existed, which records none, with `prefs.accent` `orange` SHALL load with an empty accent, because `orange` was then the default and not a choice. This SHALL happen once: every save by this version records the current version, so an `orange` picked afterwards SHALL stay orange, also after a restart and after resetting the app's data.

#### Scenario: Orange saved before themes existed
- **GIVEN** a preferences file saved before themes existed, with `accent` `orange`
- **WHEN** the app starts and the user picks Tokyo Night as the dark theme
- **THEN** the Theme swatch is selected and Tokyo Night's own accent is used

#### Scenario: Orange picked after the upgrade
- **GIVEN** the user picks the Orange preset with this version, then restarts (or resets the app's data and picks Orange again)
- **WHEN** the user picks Tokyo Night as the dark theme
- **THEN** primary buttons are still orange and the Orange swatch is selected

### Requirement: The theme selects mark the theme in use
Each theme select SHALL show and mark the theme actually in use for its appearance, whatever id is saved. The two selects and the Theme swatch SHALL appear wherever the appearance controls do, in Settings and in first-run setup.

#### Scenario: Saved id is unknown
- **GIVEN** `prefs.theme_dark` names no bundled theme
- **WHEN** the user opens the Dark theme select
- **THEN** QueryMT Dark is marked, and the saved value is unchanged until the user picks a theme

#### Scenario: First-run setup
- **WHEN** the setup screen shows the appearance controls
- **THEN** it offers the Dark theme and Light theme selects and the Theme swatch
