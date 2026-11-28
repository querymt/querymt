//! Settings > Appearance > Text: the interface and code typefaces and sizes.
//!
//! Each of the four fields is the user's setting (`Prefs`), else the theme's,
//! else the app default (`theme::apply_with`); the controls here write the
//! user's setting and re-apply the theme, so a change shows at once. A family
//! pick is only ever a family the machine has (or a bundled one), because
//! GPUI panics laying out text in one it has not.

use std::{ops::Range, sync::Arc};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::QmtApp;
use super::kit::{self, flat_btn};
use super::settings::{settings_card, simple_row};
use crate::store::Prefs;
use crate::theme::{
    self, FontCatalog, Installed, MONO_SIZE_RANGE, Palette, Typography, UI_SIZE_RANGE, clamp_size,
};

/// Step of the − / + buttons, in px.
pub(super) const SIZE_STEP: f32 = 0.5;
/// Rows of a font menu that show before it scrolls.
const MENU_ROWS: usize = 8;
/// Height of a font menu row, as a design length (it grows with the rem).
const ROW_PX: f32 = 30.;

/// Which typeface a control edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FontField {
    /// The interface (proportional) font.
    Ui,
    /// The code (monospace) font.
    Code,
}

impl FontField {
    const ALL: [FontField; 2] = [FontField::Ui, FontField::Code];

    fn key(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Code => "code",
        }
    }

    pub(super) fn range(self) -> (f32, f32) {
        match self {
            Self::Ui => UI_SIZE_RANGE,
            Self::Code => MONO_SIZE_RANGE,
        }
    }

    /// The preset chips: size and label.
    fn presets(self) -> &'static [(f32, &'static str)] {
        match self {
            Self::Ui => &[
                (14., "Small"),
                (16., "Default"),
                (18., "Large"),
                (20., "Larger"),
            ],
            Self::Code => &[(12., "12"), (13., "13"), (14., "14"), (16., "16")],
        }
    }

    fn pref_family(self, prefs: &Prefs) -> Option<&str> {
        match self {
            Self::Ui => prefs.ui_font_family.as_deref(),
            Self::Code => prefs.mono_font_family.as_deref(),
        }
    }

    fn pref_size(self, prefs: &Prefs) -> Option<f32> {
        match self {
            Self::Ui => prefs.ui_font_size,
            Self::Code => prefs.mono_font_size,
        }
    }

    fn set_family(self, prefs: &mut Prefs, family: Option<String>) {
        match self {
            Self::Ui => prefs.ui_font_family = family,
            Self::Code => prefs.mono_font_family = family,
        }
    }

    fn set_size(self, prefs: &mut Prefs, size: Option<f32>) {
        match self {
            Self::Ui => prefs.ui_font_size = size,
            Self::Code => prefs.mono_font_size = size,
        }
    }

    fn family_of(self, t: &Typography) -> SharedString {
        match self {
            Self::Ui => t.ui_family.clone(),
            Self::Code => t.mono_family.clone(),
        }
    }

    fn size_of(self, t: &Typography) -> f32 {
        match self {
            Self::Ui => t.ui_size,
            Self::Code => t.mono_size,
        }
    }
}

/// `size` one step up or down, held to `range`.
pub(super) fn step_size(size: f32, delta: f32, range: (f32, f32)) -> f32 {
    clamp_size(size + delta, range).unwrap_or(size)
}

/// A typed size: a finite number inside `range`. Out-of-range text is not
/// committed while typing (a "1" on the way to "16" must not become 10).
pub(super) fn parse_size(text: &str, range: (f32, f32)) -> Option<f32> {
    text.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && *v >= range.0 && *v <= range.1)
}

/// A size as the input shows it: `16`, `16.5`.
pub(super) fn format_size(size: f32) -> String {
    format!("{size}")
}

/// What a family picker row stands for: the theme's font, or a family.
type Choice = Option<SharedString>;

/// The rows of a family menu: "Theme default" first, then the families whose
/// name contains `query` (case-insensitive). `code` lists only monospace
/// families unless `show_all`.
pub(super) fn family_choices(
    installed: &Installed,
    code: bool,
    show_all: bool,
    query: &str,
) -> Vec<Choice> {
    let query = query.trim().to_lowercase();
    let families: Vec<SharedString> = if code && !show_all {
        installed.mono_families()
    } else {
        installed.families().to_vec()
    };
    std::iter::once(None)
        .chain(
            families
                .into_iter()
                .filter(|f| query.is_empty() || f.to_lowercase().contains(&query))
                .map(Some),
        )
        .collect()
}

/// State of the Text card: the open family menu, its search, the two size
/// inputs and the code picker's "Show all fonts".
pub(super) struct TextUi {
    pub(super) menu: Option<FontField>,
    pub(super) search: Entity<InputState>,
    ui_size: Entity<InputState>,
    mono_size: Entity<InputState>,
    pub(super) show_all: bool,
    /// The size input holding focus, for its focus ring (kept from the
    /// inputs' focus and blur events; the card renders without a window).
    size_focus: Option<FontField>,
    /// Whether the menu's search holds focus (its ring), as `size_focus`.
    search_focus: bool,
    _subs: Vec<Subscription>,
}

impl TextUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<QmtApp>) -> Self {
        let now = theme::typography(cx);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search fonts"));
        let size_input = |field: FontField, window: &mut Window, cx: &mut Context<QmtApp>| {
            let value = format_size(field.size_of(&now));
            cx.new(|cx| {
                let mut state = InputState::new(window, cx).default_value(value);
                state.set_text_align(TextAlign::Center, cx);
                state
            })
        };
        let ui_size = size_input(FontField::Ui, window, cx);
        let mono_size = size_input(FontField::Code, window, cx);
        let mut subs = vec![
            cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, _, cx| {
                match ev {
                    InputEvent::Focus => this.text.search_focus = true,
                    InputEvent::Blur => this.text.search_focus = false,
                    InputEvent::Change => {}
                    _ => return,
                }
                cx.notify();
            }),
        ];
        for field in FontField::ALL {
            let input = match field {
                FontField::Ui => &ui_size,
                FontField::Code => &mono_size,
            };
            subs.push(cx.subscribe_in(
                input,
                window,
                move |this, _, ev: &InputEvent, window, cx| {
                    this.size_input_event(field, ev, window, cx)
                },
            ));
        }
        Self {
            menu: None,
            search,
            ui_size,
            mono_size,
            show_all: false,
            size_focus: None,
            search_focus: false,
            _subs: subs,
        }
    }

    fn size_input(&self, field: FontField) -> &Entity<InputState> {
        match field {
            FontField::Ui => &self.ui_size,
            FontField::Code => &self.mono_size,
        }
    }
}

impl QmtApp {
    /// The family or size of `field` in force right now (the kit theme's).
    fn effective(&self, field: FontField, cx: &App) -> (SharedString, f32) {
        let t = theme::typography(cx);
        (field.family_of(&t), field.size_of(&t))
    }

    /// Choose the family of `field`; `None`: the theme's.
    pub(super) fn set_font_family(
        &mut self,
        field: FontField,
        family: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text.menu = None;
        self.store.update(cx, |s, cx| {
            s.update_prefs(|p| field.set_family(p, family), cx)
        });
        self.apply_theme(window, cx);
    }

    /// Choose the size of `field` (held to its range; non-finite is ignored
    /// as unset); `None`: the theme's. `sync`: rewrite the input to the size
    /// now in force.
    pub(super) fn set_font_size(
        &mut self,
        field: FontField,
        size: Option<f32>,
        sync: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let size = size.and_then(|v| clamp_size(v, field.range()));
        self.store
            .update(cx, |s, cx| s.update_prefs(|p| field.set_size(p, size), cx));
        self.apply_theme(window, cx);
        if sync {
            self.sync_size_input(field, true, window, cx);
        }
    }

    /// Show the size in force in the input of `field`, unless the user is
    /// typing in it (`force` overrides that).
    fn sync_size_input(
        &mut self,
        field: FontField,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.text.size_input(field).clone();
        if !force && input.focus_handle(cx).is_focused(window) {
            return;
        }
        let want = format_size(self.effective(field, cx).1);
        if input.read(cx).value().as_ref() != want {
            input.update(cx, |s, cx| s.set_value(want, window, cx));
        }
    }

    /// After the theme changed: the inputs follow the size now in force (a
    /// theme may bring its own).
    pub(super) fn sync_size_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for field in FontField::ALL {
            self.sync_size_input(field, false, window, cx);
        }
    }

    fn size_input_event(
        &mut self,
        field: FontField,
        ev: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let raw = self.text.size_input(field).read(cx).value();
        match ev {
            InputEvent::Focus => {
                self.text.size_focus = Some(field);
                cx.notify();
            }
            InputEvent::Change => {
                // Only a complete, in-range number; and not the value the
                // input was just set to (that would pin a theme's size).
                let Some(v) = parse_size(&raw, field.range()) else {
                    return;
                };
                let (_, shown) = self.effective(field, cx);
                let pref = field.pref_size(&self.store.read(cx).prefs);
                if v != shown || pref.is_some_and(|p| p != v) {
                    self.set_font_size(field, Some(v), false, window, cx);
                }
            }
            InputEvent::PressEnter { .. } => {
                match raw.trim().parse::<f32>().ok().filter(|v| v.is_finite()) {
                    Some(v) => self.set_font_size(field, Some(v), true, window, cx),
                    None => self.sync_size_input(field, true, window, cx),
                }
            }
            // Leaving the input with unusable text puts the size in force
            // back.
            InputEvent::Blur => {
                if self.text.size_focus == Some(field) {
                    self.text.size_focus = None;
                }
                self.sync_size_input(field, true, window, cx);
                cx.notify();
            }
        }
    }

    fn toggle_font_menu(&mut self, field: FontField, window: &mut Window, cx: &mut Context<Self>) {
        if self.text.menu == Some(field) {
            self.text.menu = None;
        } else {
            self.text.menu = Some(field);
            self.text.search.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.focus(window, cx);
            });
        }
        cx.notify();
    }

    /// The "Text" card of Appearance: the four settings.
    pub(super) fn text_card(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        // Listing the families takes ~100 ms: off the render path, once.
        FontCatalog::warm(cx);
        settings_card(p)
            .child(self.font_row(FontField::Ui, true, p, cx))
            .child(self.size_row(FontField::Ui, false, p, cx))
            .child(self.font_row(FontField::Code, false, p, cx))
            .child(self.size_row(FontField::Code, false, p, cx))
    }

    fn font_row(&self, field: FontField, first: bool, p: &Palette, cx: &mut Context<Self>) -> Div {
        let (title, help) = match field {
            FontField::Ui => (
                "Interface font",
                "The typeface of the app's text. Theme default follows the theme.",
            ),
            FontField::Code => (
                "Code font",
                "The monospace typeface of code, diffs and tool output.",
            ),
        };
        let prefs = &self.store.read(cx).prefs;
        // Trimmed as `theme::apply_with` reads it.
        let chosen = field.pref_family(prefs).map(|f| f.trim().to_owned());
        let installed = FontCatalog::peek(cx);
        let open = self.text.menu == Some(field);
        let key = field.key();

        // The pill names the setting; its own typeface when the machine has
        // it (a family that is gone is not set in, GPUI would panic).
        let (label, face) = match &chosen {
            Some(name) => {
                let known = theme::DEFAULT_UI_FAMILY == name
                    || theme::DEFAULT_MONO_FAMILY == name
                    || installed.as_ref().is_some_and(|i| i.contains(name));
                if known {
                    (name.clone(), Some(SharedString::from(name.clone())))
                } else {
                    (format!("{name} (not installed)"), None)
                }
            }
            None => ("Theme default".to_string(), None),
        };
        let trigger = flat_btn(kit::keyed("font-select", key), true, true, p)
            .debug_selector(move || format!("font-select-{key}"))
            .min_w(px(176.))
            .max_w(px(260.))
            .justify_between()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .when_some(face, |el, f| el.font_family(f))
                    .child(label),
            )
            .child(kit::icon(
                if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                },
                13.,
                p.muted,
            ))
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if this.dismissed.closed_by(&format!("font:{key}"), ev) {
                    return;
                }
                this.toggle_font_menu(field, window, cx);
            }));
        let menu = open.then(|| {
            let theme_default = theme::theme_typography(theme::palette(cx).theme, cx);
            self.font_menu(field, chosen, &theme_default, installed, p, cx)
        });
        simple_row(
            title,
            help,
            div().relative().child(trigger).children(menu),
            first,
            p,
        )
    }

    /// The open family menu: search, the rows (virtualised, each set in its
    /// own face) and, for code, "Show all fonts".
    fn font_menu(
        &self,
        field: FontField,
        chosen: Option<String>,
        theme_default: &Typography,
        installed: Option<Arc<Installed>>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = field.key();
        let code = field == FontField::Code;
        let query = self.text.search.read(cx).value();
        let show_all = self.text.show_all;
        let hover = p.card_hover;
        let p = *p;

        let search = kit::field_shell(self.text.search_focus, &p)
            .h(px(32.))
            .mb_1()
            .child(kit::icon(IconName::Search, 14., p.dim))
            .child(
                Input::new(&self.text.search)
                    .appearance(false)
                    .aria_label("Search fonts")
                    .flex_1(),
            );

        let body: AnyElement = match &installed {
            None => div()
                .px_2()
                .py_2()
                .text_size(kit::rpx(12.))
                .text_color(p.dim)
                .child("Loading fonts…")
                .into_any_element(),
            Some(installed) => {
                let choices = Arc::new(family_choices(installed, code, show_all, &query));
                let shown = choices.len().min(MENU_ROWS);
                let default_name = field.family_of(theme_default);
                let chosen = chosen.clone();
                let rows = {
                    let choices = choices.clone();
                    cx.processor(move |_: &mut QmtApp, range: Range<usize>, _, cx| {
                        range
                            .map(|ix| {
                                let choice = choices[ix].clone();
                                font_option(
                                    field,
                                    choice,
                                    &default_name,
                                    chosen.as_deref(),
                                    hover,
                                    &p,
                                    cx,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                };
                let list = uniform_list(kit::keyed("font-rows", key), choices.len(), rows)
                    .h(kit::rpx(ROW_PX) * shown as f32);
                let pending = code && !show_all && !installed.mono_ready();
                v_flex()
                    .child(list)
                    .when(choices.len() == 1 && !query.is_empty(), |el| {
                        el.child(note("No matching fonts", &p))
                    })
                    .when(pending, |el| {
                        el.child(note("Looking for monospace fonts…", &p))
                    })
                    .into_any_element()
            }
        };

        let footer = (code && !cfg!(target_family = "wasm")).then(|| {
            h_flex()
                .mt_1()
                .px_2()
                .py_1()
                .gap_2()
                .justify_between()
                .border_t_1()
                .border_color(p.border)
                .text_size(kit::rpx(12.))
                .child(div().child("Show all fonts"))
                .child(
                    kit::switch("font-show-all")
                        .checked(show_all)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.text.show_all = *v;
                            cx.notify();
                        })),
                )
        });

        deferred(
            v_flex()
                .id(kit::keyed("font-menu", key))
                .debug_selector(move || format!("font-menu-{key}"))
                .absolute()
                .top(px(36.))
                .right_0()
                .w(px(272.))
                .p_1()
                .rounded(px(10.))
                .bg(p.popover)
                .border_1()
                .border_color(p.border)
                .when(p.overlay_shadow(), |el| el.shadow_lg())
                .occlude()
                .on_mouse_down_out(cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    if this.text.menu.take().is_some() {
                        this.dismissed.record(format!("font:{key}"), ev);
                    }
                    cx.notify();
                }))
                .child(search)
                .child(body)
                .children(footer),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn size_row(&self, field: FontField, first: bool, p: &Palette, cx: &mut Context<Self>) -> Div {
        let (title, help) = match field {
            FontField::Ui => (
                "Interface size",
                "Scales all text and the controls around it, like zoom. In px.",
            ),
            FontField::Code => (
                "Code size",
                "Size of code, diffs and tool output, whatever the interface size. In px.",
            ),
        };
        let key = field.key();
        let (lo, hi) = field.range();
        let pref = field.pref_size(&self.store.read(cx).prefs);
        let (_, size) = self.effective(field, cx);

        let step = |id: &'static str, icon: IconName, delta: f32, enabled: bool| {
            flat_btn(kit::keyed(id, key), enabled, true, p)
                .debug_selector(move || format!("{id}-{key}"))
                .child(kit::icon(icon, 13., p.text))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let (_, now) = this.effective(field, cx);
                    let next = step_size(now, delta, field.range());
                    this.set_font_size(field, Some(next), true, window, cx);
                }))
        };
        let stepper = h_flex()
            .gap_1()
            .items_center()
            .child(step(
                "font-size-dec",
                IconName::Minus,
                -SIZE_STEP,
                size > lo,
            ))
            .child(
                kit::field_shell(self.text.size_focus == Some(field), p)
                    .w(px(64.))
                    .h(px(32.))
                    .child(
                        Input::new(self.text.size_input(field))
                            .appearance(false)
                            .aria_label(title)
                            .flex_1(),
                    ),
            )
            .child(step("font-size-inc", IconName::Plus, SIZE_STEP, size < hi))
            .child(
                flat_btn(kit::keyed("font-size-reset", key), pref.is_some(), true, p)
                    .debug_selector(move || format!("font-size-reset-{key}"))
                    .ml_1()
                    .child("Reset")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_font_size(field, None, true, window, cx);
                    })),
            );
        let preset_id: &'static str = match field {
            FontField::Ui => "font-size-preset-ui",
            FontField::Code => "font-size-preset-code",
        };
        let chips = kit::segment_bar(p).children(field.presets().iter().enumerate().map(
            |(ix, (value, label))| {
                let value = *value;
                kit::segment((preset_id, ix), size == value, p)
                    .label(*label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_font_size(field, Some(value), true, window, cx);
                    }))
            },
        ));
        simple_row(
            title,
            help,
            v_flex().items_end().gap_2().child(stepper).child(chips),
            first,
            p,
        )
    }
}

fn note(text: &'static str, p: &Palette) -> Div {
    div()
        .px_2()
        .py_1p5()
        .text_size(kit::rpx(12.))
        .text_color(p.dim)
        .child(text)
}

/// One row of a family menu: "Theme default" (with the family it means), or
/// a family set in its own face.
fn font_option(
    field: FontField,
    choice: Choice,
    default_name: &SharedString,
    chosen: Option<&str>,
    hover: Hsla,
    p: &Palette,
    cx: &mut Context<QmtApp>,
) -> AnyElement {
    let key = field.key();
    let selected = match (&choice, chosen) {
        (None, None) => true,
        (Some(f), Some(c)) => f.as_ref() == c,
        _ => false,
    };
    let name = choice
        .as_ref()
        .map_or_else(|| "theme-default".to_string(), |f| f.to_string());
    let picked = choice.as_ref().map(|f| f.to_string());
    let (text, face): (SharedString, Option<SharedString>) = match &choice {
        None => ("Theme default".into(), None),
        Some(f) => (f.clone(), Some(f.clone())),
    };
    let id_name = name.clone();
    h_flex()
        .id(kit::keyed("font-option", format!("{key}-{name}")))
        .debug_selector(move || format!("font-option-{key}-{id_name}"))
        .h(kit::rpx(ROW_PX))
        .px_2()
        .gap_2()
        .flex_none()
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(kit::rpx(12.))
        .hover(move |s| s.bg(hover))
        .child(
            div()
                .flex_1()
                .truncate()
                .when_some(face, |el, f| el.font_family(f))
                .child(text),
        )
        .when(choice.is_none(), |el| {
            el.child(
                div()
                    .flex_none()
                    .text_color(p.dim)
                    .child(default_name.clone()),
            )
        })
        .when(selected, |el| {
            el.child(kit::icon(IconName::Check, 13., p.accent))
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.set_font_family(field, picked.clone(), window, cx)
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{Choice, FontField, SIZE_STEP, family_choices, format_size, parse_size, step_size};
    use crate::store::AgentsStore;
    use crate::theme::Appearance;
    use crate::theme::{self, FontCatalog, Installed, MONO_SIZE_RANGE, Typography, UI_SIZE_RANGE};
    use crate::ui::Page;
    use crate::ui::QmtApp;
    use crate::ui::settings::SettingsTab;
    use gpui_kit::component::Theme;
    use gpui_kit::component::input::InputEvent;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, Modifiers, TestAppContext, VisualTestContext};
    use gpui_kit::{AppContext as _, Bounds, Entity, Pixels, point, px};

    const SANS: &str = "Test Sans";
    const MONO: &str = "Test Mono";

    // ---- the pure pieces

    #[::core::prelude::v1::test]
    fn steps_move_by_half_a_pixel_and_stop_at_the_range() {
        assert_eq!(step_size(16., SIZE_STEP, UI_SIZE_RANGE), 16.5);
        assert_eq!(step_size(16., -SIZE_STEP, UI_SIZE_RANGE), 15.5);
        assert_eq!(step_size(32., SIZE_STEP, UI_SIZE_RANGE), 32.);
        assert_eq!(step_size(10., -SIZE_STEP, UI_SIZE_RANGE), 10.);
        assert_eq!(step_size(8., -SIZE_STEP, MONO_SIZE_RANGE), 8.);
        assert!(step_size(f32::NAN, SIZE_STEP, UI_SIZE_RANGE).is_nan());
    }

    #[::core::prelude::v1::test]
    fn typed_sizes_commit_only_when_complete_and_in_range() {
        let r = UI_SIZE_RANGE;
        assert_eq!(parse_size("16", r), Some(16.));
        assert_eq!(parse_size(" 17.5 ", r), Some(17.5));
        assert_eq!(parse_size("1", r), None, "on the way to 16");
        assert_eq!(parse_size("40", r), None);
        assert_eq!(parse_size("", r), None);
        assert_eq!(parse_size("abc", r), None);
        assert_eq!(parse_size("NaN", r), None);
        assert_eq!(parse_size("inf", r), None);
        assert_eq!(format_size(16.), "16");
        assert_eq!(format_size(16.5), "16.5");
    }

    #[::core::prelude::v1::test]
    fn the_menus_list_theme_default_first_and_filter_by_name() {
        let mut installed = Installed::new(["Zed Sans", "Alpha Mono", "beta"].map(String::from));
        let names = |rows: Vec<Choice>| -> Vec<String> {
            rows.into_iter()
                .map(|c| c.map_or("<default>".into(), |f| f.to_string()))
                .collect()
        };
        assert_eq!(
            names(family_choices(&installed, false, false, "")),
            [
                "<default>",
                "Alpha Mono",
                "beta",
                "IBM Plex Mono",
                "IBM Plex Sans",
                "Zed Sans"
            ]
        );
        assert_eq!(
            names(family_choices(&installed, false, false, " MONO")),
            ["<default>", "Alpha Mono", "IBM Plex Mono"]
        );
        // Code: only monospace families, until "Show all fonts".
        assert_eq!(
            names(family_choices(&installed, true, false, "")),
            ["<default>", "IBM Plex Mono"]
        );
        assert_eq!(names(family_choices(&installed, true, true, "")).len(), 6);
        installed = Installed::new(Vec::new());
        assert_eq!(
            names(family_choices(&installed, false, false, "nope")),
            ["<default>"]
        );
    }

    // ---- the page

    fn page(
        cx: &mut TestAppContext,
        prefs: impl FnOnce(&mut crate::store::Prefs),
    ) -> (AnyWindowHandle, Entity<QmtApp>, Entity<AgentsStore>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            theme::apply(Appearance::builtin(true), cx);
            // The machine's families: no enumeration in tests.
            FontCatalog::seed(cx, &[SANS, MONO, "Other Sans"], &[MONO]);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        prefs(&mut store.prefs);
        let (handle, app, store) = cx.update(|cx| {
            let store = cx.new(|_| store);
            let (handle, app) = gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store.clone(), window, cx))
            })
            .unwrap();
            (handle, app, store)
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.apply_theme(window, cx);
                app.go(Page::Settings, cx);
                app.settings_tab = SettingsTab::Appearance;
            });
            window.render_frame(cx);
        })
        .unwrap();
        (handle, app, store)
    }

    fn render(cx: &mut TestAppContext, handle: AnyWindowHandle) {
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn bounds(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        selector: &str,
    ) -> Option<Bounds<Pixels>> {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        VisualTestContext::from_window(handle, cx).debug_bounds(selector)
    }

    /// Click `selector`, scrolling the page down to it first when it is
    /// below the window's bottom.
    fn click(cx: &mut TestAppContext, handle: AnyWindowHandle, selector: &str) {
        let window = cx
            .update_window(handle, |_, window, _| window.viewport_size().height)
            .unwrap();
        for _ in 0..40 {
            let b = bounds(cx, handle, selector)
                .unwrap_or_else(|| panic!("{selector} is not on screen"));
            if b.bottom() <= window {
                break;
            }
            VisualTestContext::from_window(handle, cx).simulate_event(gpui_kit::ScrollEvent {
                position: point(px(700.), px(200.)),
                delta: gpui_kit::ScrollDelta::Pixels(point(px(0.), px(-80.))),
                ..Default::default()
            });
            render(cx, handle);
        }
        let at = bounds(cx, handle, selector).expect("on screen").center();
        VisualTestContext::from_window(handle, cx).simulate_click(at, Modifiers::none());
        render(cx, handle);
    }

    fn prefs(cx: &mut TestAppContext, store: &Entity<AgentsStore>) -> crate::store::Prefs {
        store.read_with(cx, |s, _| s.prefs.clone())
    }

    fn kit_type(cx: &mut TestAppContext) -> Typography {
        cx.update(|cx| theme::typography(cx))
    }

    #[gpui_kit::test]
    fn picking_an_interface_font_updates_prefs_and_the_kit_theme(cx: &mut TestAppContext) {
        let (handle, _, store) = page(cx, |_| {});
        assert_eq!(kit_type(cx).ui_family, "IBM Plex Sans");
        click(cx, handle, "font-select-ui");
        assert!(
            bounds(cx, handle, "font-menu-ui").is_some(),
            "the menu opened"
        );
        click(cx, handle, &format!("font-option-ui-{SANS}"));
        assert_eq!(prefs(cx, &store).ui_font_family.as_deref(), Some(SANS));
        assert_eq!(kit_type(cx).ui_family, SANS);
        assert_eq!(cx.update(|cx| Theme::global(cx).font_family.clone()), SANS);
        assert!(
            bounds(cx, handle, "font-menu-ui").is_none(),
            "the menu closed"
        );
        // Back to the theme's.
        click(cx, handle, "font-select-ui");
        click(cx, handle, "font-option-ui-theme-default");
        assert_eq!(prefs(cx, &store).ui_font_family, None);
        assert_eq!(kit_type(cx).ui_family, "IBM Plex Sans");
    }

    #[gpui_kit::test]
    fn the_code_menu_lists_monospace_families_until_show_all(cx: &mut TestAppContext) {
        let (handle, app, store) = page(cx, |_| {});
        click(cx, handle, "font-select-code");
        assert!(bounds(cx, handle, &format!("font-option-code-{MONO}")).is_some());
        assert!(bounds(cx, handle, "font-option-code-IBM Plex Mono").is_some());
        assert!(bounds(cx, handle, &format!("font-option-code-{SANS}")).is_none());
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.text.show_all = true;
                cx.notify();
            })
        });
        render(cx, handle);
        assert!(bounds(cx, handle, &format!("font-option-code-{SANS}")).is_some());
        click(cx, handle, &format!("font-option-code-{MONO}"));
        assert_eq!(prefs(cx, &store).mono_font_family.as_deref(), Some(MONO));
        assert_eq!(kit_type(cx).mono_family, MONO);
        // The code font leaves the interface font alone.
        assert_eq!(kit_type(cx).ui_family, "IBM Plex Sans");
    }

    #[gpui_kit::test]
    fn the_size_steppers_presets_and_reset_apply_at_once(cx: &mut TestAppContext) {
        let (handle, app, store) = page(cx, |_| {});
        click(cx, handle, "font-size-inc-ui");
        assert_eq!(prefs(cx, &store).ui_font_size, Some(16.5));
        assert_eq!(kit_type(cx).ui_size, 16.5);
        assert_eq!(cx.update(|cx| Theme::global(cx).font_size), px(16.5));
        click(cx, handle, "font-size-dec-ui");
        click(cx, handle, "font-size-dec-ui");
        assert_eq!(prefs(cx, &store).ui_font_size, Some(15.5));
        let shown = |cx: &mut TestAppContext, field| {
            cx.update(|cx| {
                app.read(cx)
                    .text
                    .size_input(field)
                    .read(cx)
                    .value()
                    .to_string()
            })
        };
        assert_eq!(shown(cx, FontField::Ui), "15.5");
        // The code size is its own setting.
        click(cx, handle, "font-size-inc-code");
        assert_eq!(prefs(cx, &store).mono_font_size, Some(13.5));
        assert_eq!(cx.update(|cx| Theme::global(cx).mono_font_size), px(13.5));
        assert_eq!(cx.update(|cx| crate::ui::kit::mono_scale(cx)), 13.5 / 13.);
        // A preset chip.
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.set_font_size(FontField::Ui, Some(20.), true, window, cx)
            })
        })
        .unwrap();
        assert_eq!(kit_type(cx).ui_size, 20.);
        assert_eq!(shown(cx, FontField::Ui), "20");
        // Reset: the theme's size (the default here), unset in the prefs.
        click(cx, handle, "font-size-reset-ui");
        assert_eq!(prefs(cx, &store).ui_font_size, None);
        assert_eq!(kit_type(cx).ui_size, 16.);
        assert_eq!(shown(cx, FontField::Ui), "16");
        click(cx, handle, "font-size-reset-code");
        assert_eq!(prefs(cx, &store).mono_font_size, None);
        assert_eq!(kit_type(cx).mono_size, 13.);
        assert_eq!(cx.update(|cx| crate::ui::kit::mono_scale(cx)), 1.);
    }

    #[gpui_kit::test]
    fn typing_a_size_commits_when_complete_and_clamps_on_enter(cx: &mut TestAppContext) {
        let (handle, app, store) = page(cx, |_| {});
        // Type into the input as a user does: empty it, focus it, key in.
        let type_in = |cx: &mut TestAppContext, text: &str| {
            cx.update_window(handle, |_, window, cx| {
                let input = app.read(cx).text.size_input(FontField::Ui).clone();
                input.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.focus(window, cx);
                });
            })
            .unwrap();
            render(cx, handle);
            VisualTestContext::from_window(handle, cx).simulate_input(text);
            render(cx, handle);
        };
        type_in(cx, "18");
        assert_eq!(prefs(cx, &store).ui_font_size, Some(18.));
        assert_eq!(kit_type(cx).ui_size, 18.);
        // Not yet a size: nothing changes.
        type_in(cx, "1");
        assert_eq!(prefs(cx, &store).ui_font_size, Some(18.));
        // Enter clamps what was typed and shows it.
        type_in(cx, "99");
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.size_input_event(
                    FontField::Ui,
                    &InputEvent::PressEnter {
                        secondary: false,
                        shift: false,
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
        assert_eq!(prefs(cx, &store).ui_font_size, Some(32.));
        assert_eq!(kit_type(cx).ui_size, 32.);
    }

    #[gpui_kit::test]
    fn a_focused_size_input_shows_its_focus_ring(cx: &mut TestAppContext) {
        let (handle, app, _) = page(cx, |_| {});
        // Focus events reach listeners only in the active window.
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        let focus = |cx: &mut TestAppContext, field: Option<FontField>| {
            cx.update_window(handle, |_, window, cx| match field {
                Some(field) => {
                    let input = app.read(cx).text.size_input(field).clone();
                    input.update(cx, |s, cx| s.focus(window, cx));
                }
                None => window.blur(cx),
            })
            .unwrap();
            render(cx, handle);
            cx.run_until_parked();
            cx.update(|cx| app.read(cx).text.size_focus)
        };
        assert_eq!(cx.update(|cx| app.read(cx).text.size_focus), None);
        assert_eq!(focus(cx, Some(FontField::Code)), Some(FontField::Code));
        assert_eq!(focus(cx, Some(FontField::Ui)), Some(FontField::Ui));
        assert_eq!(focus(cx, None), None);
    }

    #[gpui_kit::test]
    fn the_settings_page_follows_the_size_without_clipping(cx: &mut TestAppContext) {
        for size in [12., 24.] {
            let (handle, _, store) = page(cx, |p| p.ui_font_size = Some(size));
            assert_eq!(kit_type(cx).ui_size, size);
            render(cx, handle);
            for selector in ["font-select-ui", "font-select-code"] {
                let b = bounds(cx, handle, selector).expect(selector);
                assert!(
                    b.size.width > px(0.) && b.size.height > px(0.),
                    "{selector} at {size}"
                );
            }
            let _ = prefs(cx, &store);
        }
    }

    #[gpui_kit::test]
    fn a_saved_family_that_is_gone_is_not_applied(cx: &mut TestAppContext) {
        let (_, _, store) = page(cx, |p| {
            p.ui_font_family = Some("Uninstalled Family".into());
            p.mono_font_family = Some("Gone Mono".into());
        });
        // Kept as saved (it may come back), but the window shows the default.
        assert_eq!(
            prefs(cx, &store).ui_font_family.as_deref(),
            Some("Uninstalled Family")
        );
        assert_eq!(kit_type(cx).ui_family, "IBM Plex Sans");
        assert_eq!(kit_type(cx).mono_family, "IBM Plex Mono");
    }
}
