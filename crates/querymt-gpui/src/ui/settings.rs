//! Settings page: General, Appearance, Keybindings, Providers, Profiles.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::mesh::panel;
use super::{QmtApp, kit};
use crate::store::{ImageSendMode, ProviderAuth, SendShortcut};
use crate::theme::{ACCENTS, Palette, accent_color, accent_dim, palette};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Appearance,
    Keybindings,
    Providers,
    Profiles,
}

const TABS: [(SettingsTab, IconName, &str, &str); 5] = [
    (
        SettingsTab::General,
        IconName::SlidersHorizontal,
        "General",
        "Sidebar, prompts and app data",
    ),
    (
        SettingsTab::Appearance,
        IconName::Palette,
        "Appearance",
        "Theme and accent color",
    ),
    (
        SettingsTab::Keybindings,
        IconName::Keyboard,
        "Keybindings",
        "Keyboard shortcuts",
    ),
    (
        SettingsTab::Providers,
        IconName::KeyRound,
        "Providers",
        "Authentication for models and services.",
    ),
    (
        SettingsTab::Profiles,
        IconName::Bot,
        "Profiles",
        "Agent profiles on the server",
    ),
];

/// Rounded card that groups [`simple_row`]s (desktop `.settings-simple-row` list).
fn settings_card(p: &Palette) -> Div {
    v_flex()
        .min_w_0()
        .rounded(px(12.))
        .overflow_hidden()
        .bg(p.card)
}

/// One row of a [`settings_card`]: title and help on the left, control on the right.
fn simple_row(
    title: &'static str,
    desc: impl Into<SharedString>,
    control: impl IntoElement,
    first: bool,
    p: &Palette,
) -> Div {
    let (subtle_border, _) = subtle(p);
    h_flex()
        .min_h(px(59.))
        .justify_between()
        .gap_4()
        .px(px(14.4))
        .py(px(11.5))
        .when(!first, |el| el.border_t_1().border_color(subtle_border))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(13.4))
                        .font_semibold()
                        .text_color(p.text)
                        .child(title),
                )
                .child(
                    div()
                        .mt(px(3.2))
                        .text_size(px(12.))
                        .text_color(p.muted)
                        .child(desc.into()),
                ),
        )
        .child(control)
}

/// Collapsible section below a settings card (desktop `.settings-advanced`):
/// a quiet trigger row with title, help and chevron, then `content` when open.
fn disclosure(
    id: &'static str,
    title: &'static str,
    desc: &'static str,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    content: Option<Div>,
    p: &Palette,
) -> Div {
    let (subtle_border, subtle_surface) = subtle(p);
    let (hover_bg, hover_fg) = (subtle_surface, p.text);
    v_flex()
        .border_t_1()
        .border_color(subtle_border)
        .pt(px(5.6))
        .child(
            h_flex()
                .id(id)
                .w_full()
                .justify_between()
                .gap_4()
                .px(px(10.4))
                .py(px(9.6))
                .rounded(px(8.))
                .cursor_pointer()
                .text_color(p.muted)
                .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
                .child(
                    v_flex()
                        .child(div().text_size(px(12.5)).font_semibold().child(title))
                        .child(
                            div()
                                .mt(px(2.4))
                                .text_size(px(10.9))
                                .text_color(p.dim)
                                .child(desc),
                        ),
                )
                .child(kit::icon(
                    if open {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    },
                    16.,
                    p.muted,
                ))
                .on_click(on_toggle),
        )
        .children(content.map(|c| c.mt(px(5.6))))
}

/// Title and help above a group of settings that is not a card row.
fn section_heading(title: &'static str, desc: &'static str, p: &Palette) -> Div {
    v_flex()
        .child(
            div()
                .text_size(px(13.4))
                .font_semibold()
                .text_color(p.text)
                .child(title),
        )
        .child(
            div()
                .mt(px(3.2))
                .text_size(px(12.))
                .text_color(p.muted)
                .child(desc),
        )
}

const THEME_PREVIEW_H: f32 = 112.;

/// Miniature app window drawn in `pal`: sidebar, header, text lines and a
/// composer with the accent dot.
fn theme_mock(pal: &Palette, accent: &str) -> Div {
    let bar = |w: f32| div().h(px(6.)).w(px(w)).rounded_full().bg(pal.border);
    // One turn, like the transcript: the user bubble sits right on the accent
    // tint (`Palette::accent_dim`), the assistant reply is plain text lines.
    let user_bubble = h_flex().justify_end().child(
        h_flex()
            .h(px(12.))
            .w(px(48.))
            .rounded(px(4.))
            .bg(accent_dim(accent, pal.dark)),
    );
    h_flex()
        .size_full()
        .rounded(px(8.))
        .border_1()
        .border_color(pal.border)
        .bg(pal.bg)
        .overflow_hidden()
        .child(
            v_flex()
                .h_full()
                .w(relative(0.26))
                .flex_none()
                .gap(px(6.))
                .p(px(8.))
                .bg(pal.card)
                .child(div().size(px(6.)).rounded(px(2.)).bg(pal.border))
                .child(bar(34.))
                .child(bar(26.))
                .child(bar(22.)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .gap(px(6.))
                .p(px(8.))
                .child(user_bubble)
                .child(bar(72.))
                .child(bar(52.))
                .child(div().flex_1())
                .child(
                    h_flex()
                        .h(px(18.))
                        .justify_end()
                        .pr(px(5.))
                        .rounded(px(5.))
                        .border_1()
                        .border_color(pal.border)
                        .bg(pal.surface)
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(accent_color(accent, pal.dark)),
                        ),
                ),
        )
}

/// Preview for a theme mode: light, dark, or (System) light with its right
/// half shown in dark.
fn theme_preview(mode: Option<bool>, accent: &str, p: &Palette) -> Div {
    let (light, dark) = (Palette::light(), Palette::dark());
    let base = div().relative().w_full().h(px(THEME_PREVIEW_H));
    match mode {
        Some(true) => base.child(theme_mock(&dark, accent)),
        Some(false) => base.child(theme_mock(&light, accent)),
        None => base.child(theme_mock(&light, accent)).child(
            // The dark mock is twice the half's width and anchored right,
            // so only its right half shows.
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(relative(0.5))
                .overflow_hidden()
                .rounded_r(px(8.))
                .border_l_1()
                .border_color(p.border_strong)
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .w(relative(2.))
                        .child(theme_mock(&dark, accent)),
                ),
        ),
    }
}

/// [`action_btn`] in the destructive tone (Clear API key, Reset app data).
fn danger_btn(id: impl Into<ElementId>, enabled: bool, p: &Palette) -> Stateful<Div> {
    let danger = p.danger;
    action_btn(id, enabled, true, p)
        .border_color(danger.opacity(0.35))
        .bg(danger.opacity(0.1))
        .text_color(danger)
        .when(enabled, |el| {
            el.hover(move |s| {
                s.border_color(danger.opacity(0.65))
                    .bg(danger.opacity(0.16))
            })
        })
}

/// Key combination and what it does, for the Keybindings tab.
type Binding = (Vec<&'static str>, &'static str);

fn keycap(k: &str, p: &Palette) -> Div {
    div()
        .px_1p5()
        .py(px(1.))
        .min_w(px(22.))
        .text_center()
        .rounded(px(5.))
        .bg(p.card)
        .border_1()
        .border_color(p.border)
        .font_family(kit::MONO)
        .text_xs()
        .child(k.to_string())
}

impl QmtApp {
    pub(super) fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let tab = self.settings_tab;

        let nav = v_flex()
            .w(px(220.))
            .flex_none()
            .gap_0p5()
            .children(TABS.iter().map(|(t, icon, label, desc)| {
                let t = *t;
                let active = t == tab;
                let hover = p.card;
                h_flex()
                    .id(*label)
                    .gap_2p5()
                    .items_start()
                    .px_3()
                    .py_2()
                    .rounded(px(10.))
                    .cursor_pointer()
                    .when(active, |el| el.bg(p.card))
                    .hover(move |s| s.bg(hover))
                    .child(div().pt_0p5().child(kit::icon(
                        *icon,
                        15.,
                        if active { p.accent } else { p.muted },
                    )))
                    .child(
                        // Constrained so long descriptions wrap inside the item.
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_semibold().child(*label))
                            .child(div().text_xs().text_color(p.dim).child(*desc)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_tab = t;
                        cx.notify();
                    }))
            }));

        let body = match tab {
            SettingsTab::General => self.settings_general(&p, cx).into_any_element(),
            SettingsTab::Appearance => self.settings_appearance(&p, cx).into_any_element(),
            SettingsTab::Keybindings => self.settings_keys(&p, cx).into_any_element(),
            SettingsTab::Providers => self.settings_providers(&p, cx).into_any_element(),
            SettingsTab::Profiles => self.settings_profiles(&p, cx).into_any_element(),
        };
        let (title, desc) = TABS
            .iter()
            .find(|x| x.0 == tab)
            .map(|x| (x.2, x.3))
            .unwrap_or_default();

        div().w_full().flex().justify_center().px_8().py_6().child(
            v_flex()
                .w_full()
                .max_w(px(1040.))
                .gap_5()
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_2xl().font_bold().child("Settings"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.muted)
                                .child("Appearance, behavior, provider access and profiles."),
                        ),
                )
                .child(
                    h_flex().items_start().gap_6().child(nav).child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(
                                h_flex()
                                    .items_start()
                                    .justify_between()
                                    .gap_4()
                                    .child(
                                        v_flex()
                                            .child(div().text_lg().font_semibold().child(title))
                                            .child(div().text_sm().text_color(p.muted).child(desc)),
                                    )
                                    .when(tab == SettingsTab::Providers, |el| {
                                        let store = self.store.read(cx);
                                        let agent = store.primary_agent();
                                        let loading = agent
                                            .as_ref()
                                            .is_some_and(|a| store.is_pending(a, "extras"));
                                        el.child(
                                            kit::loading_icon_button(
                                                "prov-refresh",
                                                IconName::RefreshCw,
                                                "Refresh providers",
                                                loading,
                                                32.,
                                            )
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    if let Some(a) = agent.clone() {
                                                        this.store.update(cx, |s, cx| {
                                                            s.refresh_extras(&a, cx)
                                                        });
                                                    }
                                                }),
                                            ),
                                        )
                                    }),
                            )
                            .child(body),
                    ),
                ),
        )
    }

    fn settings_general(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let prefs = self.store.read(cx).prefs.clone();

        let sidebar = settings_card(p)
            .child(simple_row(
                "Show pinned sessions in sidebar",
                "Keep pinned sessions above Open sessions.",
                Switch::new("sw-pinned")
                    .checked(prefs.show_pinned_sidebar)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        let v = *v;
                        this.store.update(cx, |s, cx| {
                            s.update_prefs(|p| p.show_pinned_sidebar = v, cx)
                        });
                    })),
                true,
                p,
            ))
            .child(simple_row(
                "Collapse sidebar",
                "Show only icons in the left panel. Toggle anytime with Ctrl+B.",
                Switch::new("sw-collapse")
                    .checked(prefs.sidebar_collapsed)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        let v = *v;
                        this.store
                            .update(cx, |s, cx| s.update_prefs(|p| p.sidebar_collapsed = v, cx));
                    })),
                false,
                p,
            ));

        // ---- Advanced: prompt encoding
        let advanced_open = self.general_advanced_open;
        let advanced = advanced_open.then(|| {
            let modes = [
                (ImageSendMode::Image, "Native image"),
                (ImageSendMode::Resource, "Embedded resource"),
            ];
            let encoding = kit::segment_bar(p).children(modes.into_iter().map(|(mode, label)| {
                let id = match mode {
                    ImageSendMode::Image => "image-mode-image",
                    ImageSendMode::Resource => "image-mode-resource",
                };
                kit::segment(id, prefs.image_send_mode == mode, p)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store
                            .update(cx, |s, cx| s.update_prefs(|p| p.image_send_mode = mode, cx))
                    }))
            }));
            settings_card(p).child(simple_row(
                "Image attachment encoding",
                "Send images as native ACP image blocks or embedded resources.",
                encoding,
                true,
                p,
            ))
        });
        let advanced = disclosure(
            "general-advanced",
            "Advanced",
            "How prompts are sent to agents.",
            advanced_open,
            cx.listener(|this, _, _, cx| {
                this.general_advanced_open = !this.general_advanced_open;
                cx.notify();
            }),
            advanced,
            p,
        );

        // ---- Danger zone: reset app data
        let danger_open = self.general_danger_open;
        let danger = danger_open.then(|| {
            settings_card(p).child(simple_row(
                "Reset app data",
                "Removes configured agents and preferences stored by this app, then starts \
                 over like a fresh install. Agent sessions, provider sign-ins and project \
                 files are not touched.",
                danger_btn("reset-app-data", true, p)
                    .child("Reset app data")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_reset = true;
                        cx.notify();
                    })),
                true,
                p,
            ))
        });
        let danger = disclosure(
            "general-danger",
            "Danger zone",
            "Irreversible actions for app data.",
            danger_open,
            cx.listener(|this, _, _, cx| {
                this.general_danger_open = !this.general_danger_open;
                cx.notify();
            }),
            danger,
            p,
        );

        v_flex()
            .gap(px(12.8))
            .child(sidebar)
            .child(advanced)
            .child(danger)
    }

    fn settings_appearance(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let prefs = self.store.read(cx).prefs.clone();
        let accent = if prefs.accent.is_empty() {
            "orange".to_string()
        } else {
            prefs.accent.clone()
        };

        // ---- Application theme: one preview card per mode.
        let modes = [
            (None, "theme-system", "System"),
            (Some(false), "theme-light", "Light"),
            (Some(true), "theme-dark", "Dark"),
        ];
        let (hover, accent_dim, accent_fg) = (p.card_hover, p.accent_dim, p.accent);
        let theme_cards =
            h_flex()
                .gap(px(12.))
                .children(modes.into_iter().map(|(mode, id, label)| {
                    let selected = prefs.dark == mode;
                    v_flex()
                        .id(id)
                        .flex_1()
                        .min_w_0()
                        .gap(px(8.))
                        .p(px(8.))
                        .rounded(px(12.))
                        .border_1()
                        .cursor_pointer()
                        .map(|el| {
                            if selected {
                                el.bg(accent_dim).border_color(accent_fg)
                            } else {
                                el.bg(p.card)
                                    .border_color(transparent_black())
                                    .hover(move |s| s.bg(hover))
                            }
                        })
                        .child(theme_preview(mode, &accent, p))
                        .child(
                            div()
                                .text_center()
                                .text_size(px(12.5))
                                .font_semibold()
                                .text_color(if selected { p.text } else { p.muted })
                                .child(label),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.store.update(cx, |s, cx| s.set_theme_mode(mode, cx));
                            this.apply_theme(window, cx);
                        }))
                }));

        // ---- Accent color
        let swatches =
            h_flex()
                .flex_none()
                .gap_2()
                .children(ACCENTS.iter().map(|(id, label, _, _)| {
                    let id: &'static str = id;
                    let color = accent_color(id, p.dark);
                    let selected = accent == id;
                    div()
                        .id(id)
                        .size(px(26.))
                        .rounded_full()
                        .p(px(3.))
                        .border_2()
                        .border_color(if selected { color } else { transparent_black() })
                        .cursor_pointer()
                        .tooltip(move |window, cx| kit::wrap_tip(*label).build(window, cx))
                        .child(div().size_full().rounded_full().bg(color))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.store.update(cx, |s, cx| {
                                s.update_prefs(|p| p.accent = id.to_string(), cx)
                            });
                            this.apply_theme(window, cx);
                        }))
                }));
        let accent_card = settings_card(p).child(simple_row(
            "Accent color",
            "Highlights buttons, prompts and selections. Applies instantly.",
            swatches,
            true,
            p,
        ));

        // ---- Mouse-wheel scrolling (collapsible: rarely changed).
        let scroll_open = self.appearance_scroll_open;
        let scroll = scroll_open.then(|| {
            let smoothing = if prefs.scroll_smoothing.is_empty() {
                super::smooth_scroll::DEFAULT_PRESET
            } else {
                prefs.scroll_smoothing.as_str()
            };
            let scroll_seg =
                kit::segment_bar(p).children(super::smooth_scroll::PRESETS.iter().enumerate().map(
                    |(ix, (id, label, _))| {
                        let id: &'static str = id;
                        kit::segment(("scroll-smoothing", ix), smoothing == id, p)
                            .label(*label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| {
                                    s.update_prefs(|p| p.scroll_smoothing = id.to_string(), cx)
                                })
                            }))
                    },
                ));

            let curve = if prefs.scroll_curve.is_empty() {
                super::smooth_scroll::DEFAULT_CURVE
            } else {
                prefs.scroll_curve.as_str()
            };
            let curve_seg =
                kit::segment_bar(p).children(super::smooth_scroll::CURVES.iter().enumerate().map(
                    |(ix, (id, label, _))| {
                        let id: &'static str = id;
                        kit::segment(("scroll-curve", ix), curve == id, p)
                            .label(*label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| {
                                    s.update_prefs(|p| p.scroll_curve = id.to_string(), cx)
                                })
                            }))
                    },
                ));

            let inertia = if prefs.scroll_inertia.is_empty() {
                super::smooth_scroll::DEFAULT_INERTIA
            } else {
                prefs.scroll_inertia.as_str()
            };
            let inertia_seg =
                kit::segment_bar(p).children(super::smooth_scroll::INERTIAS.iter().enumerate().map(
                    |(ix, (id, label, _))| {
                        let id: &'static str = id;
                        kit::segment(("scroll-inertia", ix), inertia == id, p)
                            .label(*label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| {
                                    s.update_prefs(|p| p.scroll_inertia = id.to_string(), cx)
                                })
                            }))
                    },
                ));

            settings_card(p)
                .child(simple_row(
                    "Smoothing",
                    "How long mouse-wheel steps take to settle.",
                    scroll_seg,
                    true,
                    p,
                ))
                .child(simple_row(
                    "Curve",
                    "Ease out starts at full speed and slows. Ease in-out ramps up first, like browsers.",
                    curve_seg,
                    false,
                    p,
                ))
                .child(simple_row(
                    "Inertia",
                    "Fast wheel spins scroll further, and the view coasts briefly after you stop.",
                    inertia_seg,
                    false,
                    p,
                ))
        });
        let scroll = disclosure(
            "appearance-scroll",
            "Mouse-wheel scrolling",
            "Smoothing, curve and inertia. Trackpads are unaffected.",
            scroll_open,
            cx.listener(|this, _, _, cx| {
                this.appearance_scroll_open = !this.appearance_scroll_open;
                cx.notify();
            }),
            scroll,
            p,
        );

        v_flex()
            .gap(px(12.8))
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_heading(
                        "Application theme",
                        "Follow the system or pick a fixed theme.",
                        p,
                    ))
                    .child(theme_cards),
            )
            .child(accent_card)
            .child(scroll)
    }

    fn settings_keys(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mac = cx.platform_info().is_mac();
        let m: &'static str = if mac { "Cmd" } else { "Ctrl" };
        let send = self.store.read(cx).prefs.send_shortcut;
        let send_keys: Vec<&'static str> = match send {
            SendShortcut::Enter => vec!["Enter"],
            SendShortcut::ShiftEnter => vec!["Shift", "Enter"],
            SendShortcut::SecondaryEnter => vec![m, "Enter"],
        };
        let newline_keys: Vec<&'static str> = match send {
            SendShortcut::Enter => vec!["Shift", "Enter"],
            _ => vec!["Enter"],
        };
        let groups: [(&str, Vec<Binding>); 3] = [
            (
                "Global",
                vec![
                    (vec![m, "N"], "New session"),
                    (vec![m, "P"], "Toggle command palette"),
                    (vec![m, "B"], "Toggle sidebar"),
                    (vec![m, "1-9"], "Open sidebar session 1-9"),
                    (vec![m, "0"], "Open sidebar session 10"),
                    (vec!["Esc"], "Close dialog or popover"),
                    (vec![m, "Q"], "Quit"),
                ],
            ),
            (
                "Session view",
                vec![
                    (vec!["Esc", "Esc"], "Cancel the running agent"),
                    (vec![m, "Z"], "Undo workspace to latest turn"),
                    (vec![m, "Shift", "Z"], "Redo workspace change"),
                ],
            ),
            (
                "Composer",
                vec![
                    (vec![m, "M"], "Open model quick picker"),
                    (vec![m, "T"], "Cycle reasoning option"),
                    (vec!["Tab"], "Cycle mode option"),
                    (send_keys, "Send prompt"),
                    (newline_keys, "New line in prompt"),
                ],
            ),
        ];
        let (subtle_border, _) = subtle(p);
        let row = |first: bool| {
            h_flex()
                .gap_4()
                .px(px(14.4))
                .py_2p5()
                .when(!first, |el| el.border_t_1().border_color(subtle_border))
        };
        let label = |text: &'static str| {
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .font_semibold()
                .text_color(p.text)
                .child(text)
        };
        let keys = |keys: &[&str]| {
            h_flex()
                .flex_none()
                .gap_1()
                .text_xs()
                .text_color(p.dim)
                .children(keys.iter().enumerate().flat_map(|(ix, k)| {
                    let plus = (ix > 0).then(|| div().child("+").into_any_element());
                    plus.into_iter()
                        .chain(std::iter::once(keycap(k, p).into_any_element()))
                }))
        };

        let send_options = [
            (SendShortcut::Enter, "send-enter", "Enter"),
            (SendShortcut::ShiftEnter, "send-shift-enter", "Shift+Enter"),
            (
                SendShortcut::SecondaryEnter,
                "send-secondary-enter",
                if mac { "Cmd+Enter" } else { "Ctrl+Enter" },
            ),
        ];
        let mut send_picker = Some(kit::segment_bar(p).children(send_options.into_iter().map(
            |(value, id, text)| {
                kit::segment(id, send == value, p)
                    .label(text)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store
                            .update(cx, |s, cx| s.update_prefs(|p| p.send_shortcut = value, cx))
                    }))
            },
        )));

        v_flex()
            .gap(px(12.8))
            .children(groups.into_iter().map(|(title, entries)| {
                let composer = title == "Composer";
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .px_1()
                            .text_size(px(13.4))
                            .font_semibold()
                            .text_color(p.text)
                            .child(title),
                    )
                    .child(
                        settings_card(p)
                            .children(entries.into_iter().enumerate().map(|(ix, (combo, text))| {
                                row(ix == 0).child(label(text)).child(keys(&combo))
                            }))
                            .when_some(
                                composer.then(|| send_picker.take()).flatten(),
                                |el, picker| {
                                    el.child(
                                        row(false)
                                            .py_1()
                                            .child(label("Send messages with"))
                                            .child(picker),
                                    )
                                },
                            ),
                    )
            }))
    }

    fn settings_providers(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let not_running = |p: &Palette| {
            panel(p)
                .p_4()
                .text_sm()
                .text_color(p.muted)
                .child("The agent is not running, so providers cannot be shown. Open Agents to start it or see why it failed.")
                .into_any_element()
        };
        let Some(agent) = store.primary_agent() else {
            return not_running(p);
        };
        let Some(rt) = store.runtime(&agent) else {
            return not_running(p);
        };

        // Desktop `sortProvidersByAction`: attention, then setup, then connected.
        let mut providers = rt.providers.clone();
        providers.sort_by(|a, b| {
            provider_state(a)
                .rank()
                .cmp(&provider_state(b).rank())
                .then_with(|| {
                    a.display_name
                        .to_lowercase()
                        .cmp(&b.display_name.to_lowercase())
                })
        });
        let oauth = rt.oauth.clone();
        let error = rt.action_error.clone();
        let maintenance = rt.maintenance.clone();
        let failed = rt.failed_plugins.clone();
        let model_count = rt.models.len();
        let (can_set_key, can_clear_key, can_set_method) = (
            rt.supports_method("querymt/auth/setApiToken"),
            rt.supports_method("querymt/auth/clearApiToken"),
            rt.supports_method("querymt/auth/setMethod"),
        );
        let busy: Vec<(bool, bool)> = providers
            .iter()
            .map(|pr| {
                let any = ["key", "oauth", "method"]
                    .iter()
                    .any(|k| store.is_pending(&agent, &format!("{k}:{}", pr.provider)));
                (
                    any,
                    store.is_pending(&agent, &format!("method:{}", pr.provider)),
                )
            })
            .collect();
        let (refreshing_models, updating_plugins) = (
            store.is_pending(&agent, "models"),
            store.is_pending(&agent, "plugins"),
        );
        let sign_in_locked = oauth.is_some();

        let (subtle_border, subtle_surface) = subtle(p);

        // ---- overview (desktop `ProviderOverview`)
        let connected = providers
            .iter()
            .filter(|pr| provider_state(pr) == ProviderState::Connected)
            .count();
        let attention = providers
            .iter()
            .filter(|pr| provider_state(pr) == ProviderState::Attention)
            .count();
        let setup = providers.len() - connected - attention;
        let stat = |label: &'static str, value: usize, color: Hsla, first: bool| {
            v_flex()
                .flex_1()
                .min_w_0()
                .px_4()
                .py(px(12.8))
                .when(!first, |el| el.border_l_1().border_color(subtle_border))
                .child(
                    div()
                        .truncate()
                        .text_size(px(11.5))
                        .text_color(p.muted)
                        .child(label),
                )
                .child(
                    div()
                        .mt(px(3.8))
                        .font_family(kit::MONO)
                        .text_size(px(15.2))
                        .line_height(px(19.))
                        .text_color(color)
                        .child(value.to_string()),
                )
        };
        let overview = h_flex()
            .rounded(px(12.))
            .overflow_hidden()
            .bg(p.card)
            .child(stat(
                "Connected",
                connected,
                if connected > 0 { p.success } else { p.text },
                true,
            ))
            .child(stat("Needs setup", setup, p.text, false))
            .child(stat(
                "Needs attention",
                attention,
                if attention > 0 { p.warn } else { p.text },
                false,
            ))
            .child(stat("Models", model_count, p.text, false));

        // ---- provider rows (desktop `ProviderConnectionRow`)
        let count = providers.len();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (ix, pr) in providers.iter().enumerate() {
            let state = provider_state(pr);
            let (pending, method_pending) = busy[ix];
            let pkey: SharedString = pr.provider.clone().into();
            let open = self.provider_open.contains(&pr.provider);
            let signing = oauth.as_ref().is_some_and(|f| f.provider == pr.provider);
            let primary = match state {
                ProviderState::Attention if pr.supports_oauth => "Reconnect",
                ProviderState::Setup => "Set up",
                _ => "Manage",
            };

            let primary_el: AnyElement = if signing {
                let a = agent.clone();
                h_flex()
                    .gap_1()
                    .child(
                        action_btn(kit::keyed("prov-signing", pkey.clone()), false, true, p)
                            .child(kit::spinner(
                                kit::keyed("prov-spin", pkey.clone()),
                                14.,
                                p.muted,
                            ))
                            .child("Signing in…"),
                    )
                    .child(
                        kit::icon_button(
                            kit::keyed("prov-cancel", pkey.clone()),
                            IconName::CircleStop,
                            "Cancel sign-in",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.cancel_oauth(&a, cx))
                        })),
                    )
                    .into_any_element()
            } else {
                // The whole header toggles the details; only an expired OAuth
                // connection keeps an explicit Reconnect action beside the chevron.
                let reconnect = primary == "Reconnect";
                let enabled = !pending && !sign_in_locked;
                let (a, prov) = (agent.clone(), pr.provider.clone());
                h_flex()
                    .flex_none()
                    .gap_2()
                    .when(reconnect, |el| {
                        el.child(
                            action_btn(
                                kit::keyed("prov-reconnect", pkey.clone()),
                                enabled,
                                true,
                                p,
                            )
                            .text_color(p.warn)
                            .child(kit::icon(IconName::LogIn, 14., p.warn))
                            .child("Reconnect")
                            .when(enabled, |el| {
                                el.on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.store.update(cx, |s, cx| s.start_oauth(&a, &prov, cx));
                                }))
                            }),
                        )
                    })
                    .child(
                        div()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(kit::icon(
                                if open {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                },
                                15.,
                                p.muted,
                            )),
                    )
                    .into_any_element()
            };

            // Finish an OAuth flow in place (desktop opens a dialog for this).
            let oauth_box = oauth.as_ref().filter(|_| signing).map(|flow| {
                let (a, a2) = (agent.clone(), agent.clone());
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(px(8.))
                    .bg(subtle_surface)
                    .child(div().text_size(px(11.8)).text_color(p.muted).child(if flow.needs_code {
                        "A browser window opened. After approving, paste the callback URL or code here."
                    } else {
                        "A browser window opened. Approve the device there, then press Done."
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .when(flow.needs_code, |el| el.child(Input::new(&self.oauth_code).small().flex_1()))
                            .when(!flow.needs_code, |el| el.child(div().flex_1()))
                            .child(kit::button(kit::keyed("oauth-cancel", pkey.clone())).small().label("Cancel").on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| s.cancel_oauth(&a, cx))
                            })))
                            .child(kit::button(kit::keyed("oauth-done", pkey.clone())).small().primary().label("Done").on_click(cx.listener(move |this, _, window, cx| {
                                let code = this.oauth_code.read(cx).value().trim().to_string();
                                this.oauth_code.update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
                                this.store.update(cx, |s, cx| s.complete_oauth(&a2, code, cx));
                            }))),
                    )
            });

            let details = open.then(|| {
                let detail_row = |first: bool| {
                    h_flex()
                        .min_h(px(54.))
                        .justify_between()
                        .gap_4()
                        .px(px(12.))
                        .py(px(10.4))
                        .when(!first, |el| el.border_t_1().border_color(subtle_border))
                };
                let label = |title: &'static str, sub: Div| {
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(div().text_size(px(11.8)).font_semibold().text_color(p.text).child(title))
                        .child(sub.mt(px(2.5)).text_size(px(11.2)).text_color(p.muted))
                };

                // Authentication method: a pill select.
                let current = pr.preferred_method.clone().unwrap_or_else(|| "auto".into());
                let mut choices: Vec<(&'static str, &'static str)> = vec![("auto", "Auto"), ("api_key", "API key")];
                if pr.supports_oauth {
                    choices.push(("oauth", "OAuth"));
                }
                if pr.env_var_name.is_some() {
                    choices.push(("env_var", "Env var"));
                }
                let current_label = choices.iter().find(|c| c.0 == current).map_or(current.clone(), |c| c.1.to_string());
                let menu_open = self.method_menu.as_deref() == Some(pr.provider.as_str());
                let select_enabled = can_set_method && !method_pending;
                let prov = pr.provider.clone();
                let trigger = action_btn(kit::keyed("prov-method", pkey.clone()), select_enabled, true, p)
                    .min_w(px(128.))
                    .justify_between()
                    .child(current_label)
                    .child(if method_pending {
                        kit::spinner(kit::keyed("prov-method-spin", pkey.clone()), 13., p.muted).into_any_element()
                    } else {
                        kit::icon(if menu_open { IconName::ChevronUp } else { IconName::ChevronDown }, 13., p.muted).into_any_element()
                    })
                    .when(select_enabled, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.method_menu = if this.method_menu.as_deref() == Some(prov.as_str()) { None } else { Some(prov.clone()) };
                            cx.notify();
                        }))
                    });
                let menu = menu_open.then(|| {
                    let hover = p.card_hover;
                    let items: Vec<AnyElement> = choices
                        .iter()
                        .map(|(value, text)| {
                            let (a, prov, value) = (agent.clone(), pr.provider.clone(), value.to_string());
                            let selected = value == current;
                            h_flex()
                                .id(kit::keyed("prov-method-opt", format!("{}:{value}", pr.provider)))
                                .h(px(30.))
                                .px_2()
                                .gap_2()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(12.))
                                .hover(move |s| s.bg(hover))
                                .child(div().flex_1().child(*text))
                                .when(selected, |el| el.child(kit::icon(IconName::Check, 13., p.accent)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.method_menu = None;
                                    if !selected {
                                        this.store.update(cx, |s, cx| s.set_auth_method(&a, &prov, &value, cx));
                                    }
                                    cx.notify();
                                }))
                                .into_any_element()
                        })
                        .collect();
                    deferred(
                        v_flex()
                            .id(kit::keyed("prov-method-menu", pkey.clone()))
                            .absolute()
                            .top(px(36.))
                            .right_0()
                            .w(px(160.))
                            .p_1()
                            .rounded(px(10.))
                            .bg(p.surface)
                            .border_1()
                            .border_color(p.border)
                            .shadow_lg()
                            .occlude()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.method_menu = None;
                                cx.notify();
                            }))
                            .children(items),
                    )
                    .with_priority(1)
                });

                let show_set = can_set_key && pr.env_var_name.as_deref().is_some_and(|v| !v.trim().is_empty());
                let show_clear = show_set && can_clear_key && pr.has_stored_api_key;
                let oauth_connected = pr.oauth_status.as_deref() == Some("connected");
                let mut actions: Vec<AnyElement> = Vec::new();
                if pr.supports_oauth && !oauth_connected {
                    let enabled = !pending && !sign_in_locked;
                    let (a, prov) = (agent.clone(), pr.provider.clone());
                    actions.push(
                        action_btn(kit::keyed("prov-signin", pkey.clone()), enabled, true, p)
                            .child(kit::icon(IconName::LogIn, 14., p.text))
                            .child(if pr.oauth_status.as_deref() == Some("expired") { "Reconnect OAuth" } else { "Sign in with OAuth" })
                            .when(enabled, |el| el.on_click(cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.start_oauth(&a, &prov, cx)))))
                            .into_any_element(),
                    );
                }
                if pr.supports_oauth && oauth_connected {
                    let (a, prov) = (agent.clone(), pr.provider.clone());
                    actions.push(
                        action_btn(kit::keyed("prov-signout", pkey.clone()), !pending, true, p)
                            .child(kit::icon(IconName::LogOut, 14., p.text))
                            .child("Disconnect OAuth")
                            .when(!pending, |el| el.on_click(cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.logout_provider(&a, &prov, cx)))))
                            .into_any_element(),
                    );
                }
                if show_set {
                    let prov = pr.provider.clone();
                    actions.push(
                        action_btn(kit::keyed("prov-setkey", pkey.clone()), !pending, true, p)
                            .child(kit::icon(IconName::KeyRound, 14., p.text))
                            .child(if pr.has_stored_api_key { "Replace API key" } else { "Set API key" })
                            .when(!pending, |el| {
                                el.on_click(cx.listener(move |this, _, window, cx| {
                                    this.key_editing = Some(prov.clone());
                                    this.key_input.update(cx, |s, cx| {
                                        s.set_value(SharedString::default(), window, cx);
                                        s.focus(window, cx);
                                    });
                                    cx.notify();
                                }))
                            })
                            .into_any_element(),
                    );
                }
                if show_clear {
                    let (a, prov) = (agent.clone(), pr.provider.clone());
                    actions.push(
                        danger_btn(kit::keyed("prov-clearkey", pkey.clone()), !pending, p)
                            .child(kit::icon(IconName::Trash, 14., p.danger))
                            .child("Clear API key")
                            .when(!pending, |el| el.on_click(cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.clear_api_key(&a, &prov, cx)))))
                            .into_any_element(),
                    );
                }

                let editing = self.key_editing.as_deref() == Some(pr.provider.as_str());
                let editor = editing.then(|| {
                    let (a, prov) = (agent.clone(), pr.provider.clone());
                    h_flex()
                        .gap_2()
                        .px(px(12.))
                        .py(px(11.2))
                        .border_t_1()
                        .border_color(subtle_border)
                        .child(Input::new(&self.key_input).small().flex_1().mask_toggle())
                        .child(kit::button(kit::keyed("key-cancel", pkey.clone())).small().label("Cancel").on_click(cx.listener(|this, _, _, cx| {
                            this.key_editing = None;
                            cx.notify();
                        })))
                        .child(kit::button(kit::keyed("key-save", pkey.clone())).small().primary().label("Save").on_click(cx.listener(move |this, _, window, cx| {
                            let key = this.key_input.read(cx).value().trim().to_string();
                            if key.is_empty() {
                                return;
                            }
                            this.key_editing = None;
                            this.key_input.update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
                            this.store.update(cx, |s, cx| s.set_api_key(&a, &prov, key, cx));
                        })))
                });

                v_flex()
                    .mt_1()
                    .rounded(px(8.))
                    .bg(subtle_surface)
                    .child(
                        detail_row(true)
                            .child(label(
                                "Authentication method",
                                div().child(if can_set_method {
                                    "Choose the preferred credential source."
                                } else {
                                    "This agent version does not support changing the preferred auth method."
                                }),
                            ))
                            .child(div().relative().flex_none().child(trigger).children(menu)),
                    )
                    .child(detail_row(false).child(label("Provider ID", div().font_family(kit::MONO).truncate().child(pr.provider.clone()))))
                    .when_some(pr.env_var_name.clone(), |el, env| {
                        el.child(
                            detail_row(false)
                                .child(label("Environment variable", div().font_family(kit::MONO).truncate().child(env)))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.2))
                                        .text_color(if pr.has_env_api_key { p.success } else { p.dim })
                                        .child(if pr.has_env_api_key { "Available" } else { "Not set" }),
                                ),
                        )
                    })
                    .when(!actions.is_empty(), |el| {
                        el.child(h_flex().flex_wrap().gap(px(7.2)).px(px(12.)).py(px(11.2)).border_t_1().border_color(subtle_border).children(actions))
                    })
                    .children(editor)
            });

            let hover = p.card_hover;
            let strip = match state {
                ProviderState::Connected => Some(p.success),
                ProviderState::Attention => Some(p.warn),
                ProviderState::Setup => None,
            };
            // GPUI's overflow mask is rectangular, so the list can't clip its
            // rows: the first and last rows round their own outer corners.
            let (first, last) = (ix == 0, ix + 1 == count);
            let prov = pr.provider.clone();
            let has_body = oauth_box.is_some() || details.is_some();
            rows.push(
                v_flex()
                    .id(kit::keyed("prov-row", pkey.clone()))
                    .relative()
                    .min_w_0()
                    .bg(if open { hover } else { p.card })
                    .when(first, |el| el.rounded_t(px(12.)))
                    .when(last, |el| el.rounded_b(px(12.)))
                    .when(!first, |el| el.border_t_1().border_color(subtle_border))
                    .child(
                        h_flex()
                            .id(kit::keyed("prov-head", pkey.clone()))
                            .relative()
                            .min_w_0()
                            .gap_3()
                            .pl(px(18.))
                            .pr(px(10.))
                            .py(px(12.))
                            .cursor_pointer()
                            .when(first, |el| el.rounded_t(px(12.)))
                            .when(last && !has_body, |el| el.rounded_b(px(12.)))
                            .hover(move |s| s.bg(hover))
                            // Status as a short rounded bar at the leading edge
                            // (desktop draws a 2px inset strip).
                            .when_some(strip, |el, color| {
                                el.child(
                                    div()
                                        .absolute()
                                        .left(px(7.))
                                        .top(px(14.))
                                        .bottom(px(14.))
                                        .w(px(3.))
                                        .rounded_full()
                                        .bg(color),
                                )
                            })
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(13.4))
                                            .font_semibold()
                                            .text_color(p.text)
                                            .child(pr.display_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .mt(px(3.2))
                                            .truncate()
                                            .text_size(px(12.))
                                            .text_color(p.muted)
                                            .child(provider_summary(pr)),
                                    ),
                            )
                            .child(primary_el)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.provider_open.remove(&prov) {
                                    this.provider_open.insert(prov.clone());
                                }
                                this.method_menu = None;
                                cx.notify();
                            })),
                    )
                    .when(has_body, |el| {
                        el.child(
                            v_flex()
                                .min_w_0()
                                .gap(px(7.2))
                                .px(px(12.))
                                .pb(px(12.))
                                .children(oauth_box)
                                .children(details),
                        )
                    })
                    .into_any_element(),
            );
        }
        let list = if rows.is_empty() {
            panel(p)
                .p_4()
                .text_sm()
                .text_color(p.muted)
                .child("No auth-enabled providers reported by this agent.")
                .into_any_element()
        } else {
            v_flex()
                .min_w_0()
                .rounded(px(12.))
                .bg(p.card)
                .children(rows)
                .into_any_element()
        };

        // ---- Advanced maintenance (desktop `ProviderMaintenance`)
        let maint_open = self.maintenance_open;
        let content =
            maint_open.then(|| {
                let (a, a2) = (agent.clone(), agent.clone());
                let refresh = action_btn("maint-refresh", !refreshing_models, false, p)
                    .child(if refreshing_models {
                        kit::spinner("maint-refresh-spin", 14., p.muted).into_any_element()
                    } else {
                        kit::icon(IconName::RefreshCw, 14., p.text).into_any_element()
                    })
                    .child(if refreshing_models {
                        "Refreshing…"
                    } else {
                        "Refresh"
                    })
                    .when(!refreshing_models, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.refresh_models(&a, cx))
                        }))
                    });
                let update = action_btn("maint-update", !updating_plugins, false, p)
                    .when(updating_plugins, |el| {
                        el.child(kit::spinner("maint-update-spin", 14., p.muted))
                    })
                    .child(if updating_plugins {
                        "Updating…"
                    } else {
                        "Update"
                    })
                    .when(!updating_plugins, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.update_plugins(&a2, cx))
                        }))
                    });
                let models_desc = format!(
                    "{model_count} available {} for the selected agent.",
                    if model_count == 1 { "model" } else { "models" }
                );
                let failed_open = self.failed_plugins_open;
                let failed_box =
                    (!failed.is_empty()).then(|| {
                        let n = failed.len();
                        v_flex()
                            .px(px(14.4))
                            .py(px(8.8))
                            .border_t_1()
                            .border_color(subtle_border)
                            .child(
                                h_flex()
                                    .id("maint-failed")
                                    .justify_between()
                                    .gap_4()
                                    .cursor_pointer()
                                    .text_size(px(11.8))
                                    .font_semibold()
                                    .text_color(p.danger)
                                    .child(format!(
                                        "{n} plugin {}",
                                        if n == 1 {
                                            "update needs attention"
                                        } else {
                                            "updates need attention"
                                        }
                                    ))
                                    .child(kit::icon(
                                        if failed_open {
                                            IconName::ChevronUp
                                        } else {
                                            IconName::ChevronDown
                                        },
                                        14.,
                                        p.danger,
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.failed_plugins_open = !this.failed_plugins_open;
                                        cx.notify();
                                    })),
                            )
                            .when(failed_open, |el| {
                                el.child(v_flex().mt(px(8.8)).gap(px(5.6)).children(
                                    failed.iter().map(|(name, msg)| {
                                        h_flex()
                                            .items_start()
                                            .gap(px(11.2))
                                            .text_size(px(11.5))
                                            .child(
                                                div()
                                                    .min_w(px(112.))
                                                    .text_color(p.text)
                                                    .font_semibold()
                                                    .child(name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_color(p.muted)
                                                    .child(msg.clone()),
                                            )
                                    }),
                                ))
                            })
                    });
                settings_card(p)
                    .child(simple_row("Models", models_desc, refresh, true, p))
                    .child(simple_row(
                        "Plugins",
                        "Update installed provider integrations.",
                        update,
                        false,
                        p,
                    ))
                    .when_some(maintenance.clone(), |el, (msg, is_err)| {
                        let color = if is_err { p.danger } else { p.success };
                        el.child(
                            div()
                                .mx(px(14.4))
                                .my(px(11.2))
                                .px_3()
                                .py_2()
                                .rounded(px(8.))
                                .bg(color.opacity(0.1))
                                .text_size(px(12.))
                                .text_color(color)
                                .child(msg),
                        )
                    })
                    .children(failed_box)
            });
        let maintenance_el = disclosure(
            "maint-trigger",
            "Advanced maintenance",
            "Refresh model availability and update provider plugins.",
            maint_open,
            cx.listener(|this, _, _, cx| {
                this.maintenance_open = !this.maintenance_open;
                cx.notify();
            }),
            content,
            p,
        );

        v_flex()
            .gap(px(12.8))
            .child(overview)
            .when_some(error, |el, err| {
                let a = agent.clone();
                el.child(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded(px(10.))
                        .bg(p.danger.opacity(0.1))
                        .text_sm()
                        .child(kit::icon(IconName::CircleAlert, 15., p.danger))
                        .child(div().flex_1().child(err))
                        .child(
                            kit::icon_button("prov-err", IconName::X, "Dismiss").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.store.update(cx, |s, cx| s.clear_action_error(&a, cx))
                                }),
                            ),
                        ),
                )
            })
            .child(list)
            .child(maintenance_el)
            .into_any_element()
    }

    fn settings_profiles(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let profiles = store
            .primary_agent()
            .and_then(|a| store.runtime(&a))
            .map(|r| r.profiles.clone())
            .unwrap_or_default();
        if profiles.is_empty() {
            return panel(p)
                .p_4()
                .text_sm()
                .text_color(p.dim)
                .child("No profiles reported by the agent.")
                .into_any_element();
        }
        panel(p)
            .children(profiles.into_iter().enumerate().map(|(ix, pr)| {
                let source = pr
                    .source
                    .split_once(':')
                    .map(|(k, v)| (k.to_string(), v.to_string()));
                v_flex()
                    .gap_1()
                    .px_4()
                    .py_3()
                    .when(ix > 0, |el| el.border_t_1().border_color(p.border))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().font_semibold().child(pr.name.clone()))
                            .when(pr.name != pr.id, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .font_family(kit::MONO)
                                        .text_color(p.dim)
                                        .child(pr.id.clone()),
                                )
                            })
                            .child(kit::tone_chip(
                                if pr.kind == "quorum" {
                                    "multi-agent"
                                } else {
                                    "single"
                                },
                                if pr.kind == "quorum" {
                                    p.accent
                                } else {
                                    p.status_muted
                                },
                            ))
                            .child(div().flex_1())
                            .children(pr.tags.iter().map(|t| kit::chip(t.clone(), p))),
                    )
                    .when(!pr.description.is_empty(), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child(pr.description.clone()),
                        )
                    })
                    .when_some(source, |el, (kind, path)| {
                        el.child(
                            h_flex()
                                .gap_1p5()
                                .text_xs()
                                .text_color(p.dim)
                                .child(kit::icon(
                                    if kind == "embedded" {
                                        IconName::Bot
                                    } else {
                                        IconName::FileText
                                    },
                                    11.,
                                    p.dim,
                                ))
                                .child(div().font_family(kit::MONO).truncate().child(path)),
                        )
                    })
            }))
            .into_any_element()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderState {
    Attention,
    Setup,
    Connected,
}

impl ProviderState {
    fn rank(self) -> u8 {
        self as u8
    }
}

/// Desktop `providerConnectionState`.
fn provider_state(pr: &ProviderAuth) -> ProviderState {
    if matches!(pr.oauth_status.as_deref(), Some("connected"))
        || pr.has_stored_api_key
        || pr.has_env_api_key
    {
        ProviderState::Connected
    } else if pr.oauth_status.as_deref() == Some("expired") {
        ProviderState::Attention
    } else {
        ProviderState::Setup
    }
}

/// Desktop `--subtle-border` / `--subtle-surface`.
fn subtle(p: &Palette) -> (Hsla, Hsla) {
    if p.dark {
        (hsla(0., 0., 1., 0.075), rgb(0x222222).into())
    } else {
        (
            Hsla::from(rgb(0x241f31)).opacity(0.09),
            hsla(0., 0., 1., 0.72),
        )
    }
}

/// Desktop `providerConnectionSummary`.
fn provider_summary(pr: &ProviderAuth) -> String {
    match pr.oauth_status.as_deref() {
        Some("connected") => "Connected with OAuth".into(),
        _ if pr.has_stored_api_key => "Using a stored API key".into(),
        _ if pr.has_env_api_key => pr
            .env_var_name
            .as_ref()
            .map_or("Using an environment key".into(), |v| format!("Using {v}")),
        Some("expired") => "OAuth connection expired".into(),
        _ => match (&pr.preferred_method, &pr.env_var_name) {
            (Some(m), Some(v)) if m == "env_var" => format!("Set {v} to connect"),
            _ => "Authentication required".into(),
        },
    }
}

/// Desktop `.action-btn` (`compact`: `.action-btn-compact`): bordered card
/// fill, stronger border + hover fill when enabled, dimmed when not.
fn action_btn(
    id: impl Into<ElementId>,
    enabled: bool,
    compact: bool,
    p: &Palette,
) -> Stateful<Div> {
    let (hover_border, hover_bg) = (p.border_strong, p.card_hover);
    h_flex()
        .id(id)
        .flex_none()
        .justify_center()
        .gap(px(if compact { 6.4 } else { 8. }))
        .h(px(if compact { 32. } else { 36. }))
        .px(px(if compact { 12. } else { 15.2 }))
        .rounded(px(8.))
        .border_1()
        .border_color(p.border)
        .bg(p.card)
        .text_color(p.text)
        .text_size(px(if compact { 12. } else { 13.1 }))
        .font_weight(FontWeight(550.))
        .map(|el| {
            if enabled {
                el.cursor_pointer()
                    .hover(move |s| s.border_color(hover_border).bg(hover_bg))
            } else {
                el.opacity(0.5)
            }
        })
}
