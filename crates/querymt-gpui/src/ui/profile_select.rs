//! Profile picker of the Start composer's Options, in the app's select style
//! (trigger like the settings pill selects, menu like the session search):
//! the search fuzzy-matches profile name, id and description.

use gpui_kit::assets::IconName;
use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner};
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{QmtApp, kit};
use crate::store::{DEFAULT_PROFILE, Profile};
use crate::theme::Palette;
use crate::util::fuzzy_score;

#[derive(Clone, Debug, PartialEq)]
pub struct ProfileItem {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// "Default profile" first, then the agent's own profiles, as in the desktop
/// app; empty when the agent reports no profiles.
fn profile_items(profiles: &[Profile]) -> Vec<ProfileItem> {
    if profiles.is_empty() {
        return Vec::new();
    }
    let default = ProfileItem {
        id: DEFAULT_PROFILE.into(),
        name: "Default profile".into(),
        description: "Use the agent default.".into(),
    };
    std::iter::once(default)
        .chain(
            profiles
                .iter()
                .filter(|p| p.id != DEFAULT_PROFILE)
                .map(|p| ProfileItem {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    description: p.description.clone(),
                }),
        )
        .collect()
}

/// Items matching `query`, best first; equal scores keep the list order.
fn search(items: Vec<ProfileItem>, query: &str) -> Vec<ProfileItem> {
    let mut scored: Vec<(u32, ProfileItem)> = items
        .into_iter()
        .filter_map(|p| Some((fuzzy_score(query, &[&p.name, &p.id, &p.description])?, p)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, p)| p).collect()
}

/// Gap between the menu and the window edge.
const MENU_MARGIN: f32 = 8.;
/// Bounds of the scrolling row list (about 3 to 9 rows).
const MIN_LIST_H: f32 = 96.;
const MAX_LIST_H: f32 = 280.;

/// What the picker shows for the primary agent.
struct Choices {
    agent: String,
    selected: String,
    items: Vec<ProfileItem>,
}

impl QmtApp {
    fn profile_choices(&self, cx: &App) -> Option<Choices> {
        let store = self.store.read(cx);
        let agent = store.primary_agent()?;
        let items = profile_items(store.profiles(&agent));
        (!items.is_empty()).then(|| Choices {
            selected: store.launch_profile(&agent).to_string(),
            agent,
            items,
        })
    }

    /// Profiles matching the menu's search query.
    fn matched_profiles(&self, cx: &App) -> Vec<ProfileItem> {
        let query = self.profile_query.read(cx).value().to_string();
        self.profile_choices(cx)
            .map_or_else(Vec::new, |c| search(c.items, &query))
    }

    fn open_profile_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.profile_choices(cx).map(|c| c.selected);
        self.profile_menu_open = true;
        self.profile_query.update(cx, |s, cx| {
            s.set_value(SharedString::default(), window, cx);
            s.focus(window, cx);
        });
        self.profile_cursor = self
            .matched_profiles(cx)
            .iter()
            .position(|p| Some(&p.id) == selected.as_ref())
            .unwrap_or(0);
        self.profile_scroll.scroll_to_item(self.profile_cursor);
        cx.notify();
    }

    pub(super) fn close_profile_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.profile_menu_open) {
            self.app_focus.focus(window, cx);
            cx.notify();
        }
    }

    fn pick_profile(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(agent) = self.profile_choices(cx).map(|c| c.agent) {
            self.store
                .update(cx, |s, cx| s.set_launch_profile(&agent, id, cx));
        }
        self.close_profile_menu(window, cx);
    }

    /// Enter in the search box picks the highlighted row.
    pub(super) fn pick_profile_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.matched_profiles(cx).get(self.profile_cursor) {
            let id = item.id.clone();
            self.pick_profile(&id, window, cx);
        }
    }

    fn move_profile_cursor(&mut self, down: bool, cx: &mut Context<Self>) {
        let len = self.matched_profiles(cx).len();
        if len == 0 {
            return;
        }
        self.profile_cursor = if down {
            (self.profile_cursor + 1) % len
        } else {
            (self.profile_cursor + len - 1) % len
        };
        self.profile_scroll.scroll_to_item(self.profile_cursor);
        cx.notify();
    }

    /// The Profile row of the Options popover, when the agent has profiles.
    pub(super) fn profile_picker(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let choices = self.profile_choices(cx)?;
        let open = self.profile_menu_open;
        let current = choices
            .items
            .iter()
            .find(|i| i.id == choices.selected)
            .map_or_else(|| choices.selected.clone(), |i| i.name.clone());
        // Same trigger as the settings pill selects (auth method).
        let trigger = kit::action_btn("opt-profile", true, true, p)
            .w_full()
            .justify_start()
            .when(open, |el| el.border_color(p.border_strong))
            .child(kit::icon(IconName::FileCog, 14., p.muted))
            .child(div().flex_1().min_w_0().truncate().child(current))
            .child(kit::icon(
                if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                },
                13.,
                p.muted,
            ))
            .on_click(cx.listener(|this, _, window, cx| this.open_profile_menu(window, cx)));

        // Below the trigger, flipped above it when the window is too short,
        // and kept inside the window either way.
        let menu = open.then(|| {
            let positioner = Positioner::side(self.profile_trigger)
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(4.))
                .margin(px(MENU_MARGIN));
            deferred(positioner.child(self.render_profile_menu(&choices.selected, p, cx)))
                .with_priority(2)
        });
        let this = cx.entity().downgrade();
        Some(
            div()
                .w_full()
                .child(trigger)
                .children(menu)
                .on_prepaint(move |bounds, window, cx| {
                    let viewport = window.viewport_size().height;
                    let _ = this.update(cx, |this, cx| {
                        if this.profile_trigger != bounds || this.profile_viewport_h != viewport {
                            this.profile_trigger = bounds;
                            this.profile_viewport_h = viewport;
                            if this.profile_menu_open {
                                cx.notify();
                            }
                        }
                    });
                })
                .into_any_element(),
        )
    }

    /// Row-list height that lets the menu fit on the roomier side of the trigger.
    fn profile_list_max_h(&self) -> Pixels {
        let below = self.profile_viewport_h - self.profile_trigger.bottom();
        let above = self.profile_trigger.top();
        // Search box, paddings, gap, offset, window margin and frame inset.
        let chrome = px(32. + 8. + 4. + 2. + 4. + MENU_MARGIN + 16.);
        (below.max(above) - chrome).clamp(px(MIN_LIST_H), px(MAX_LIST_H))
    }

    fn render_profile_menu(
        &self,
        selected: &str,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hover = p.card_hover;
        let rows: Vec<AnyElement> = self
            .matched_profiles(cx)
            .into_iter()
            .enumerate()
            .map(|(ix, item)| {
                let checked = item.id == selected;
                let id = item.id.clone();
                h_flex()
                    .id(kit::keyed("opt-profile-row", item.id.clone()))
                    .flex_none()
                    .h(px(30.))
                    .px_2()
                    .gap_3()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .hover(move |s| s.bg(hover))
                    .when(ix == self.profile_cursor, |el| el.bg(hover))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(180.))
                            .truncate()
                            .text_color(p.text)
                            .font_weight(if checked {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .child(item.name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.2))
                            .text_color(p.dim)
                            .child(item.description),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(110.))
                            .truncate()
                            .font_family(kit::MONO)
                            .text_size(px(11.))
                            .text_color(p.dim)
                            .child(item.id),
                    )
                    .child(
                        kit::icon(IconName::Check, 13., p.accent)
                            .flex_none()
                            .when(!checked, |el| el.invisible()),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.pick_profile(&id, window, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();

        v_flex()
            .id("opt-profile-menu")
            .w(px(460.))
            .p_1()
            .gap_1()
            .rounded(px(10.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.close_profile_menu(window, cx)),
            )
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "up" => this.move_profile_cursor(false, cx),
                    "down" => this.move_profile_cursor(true, cx),
                    "escape" => this.close_profile_menu(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            // Search box like the session list's.
            .child(
                h_flex()
                    .h(px(32.))
                    .px_2p5()
                    .gap_2()
                    .rounded(px(8.))
                    .bg(p.card)
                    .border_1()
                    .border_color(p.border)
                    .child(kit::icon(IconName::Search, 14., p.dim))
                    .child(
                        Input::new(&self.profile_query)
                            .appearance(false)
                            .small()
                            .flex_1(),
                    ),
            )
            .child(
                v_flex()
                    .id("opt-profile-list")
                    .max_h(self.profile_list_max_h())
                    .overflow_y_scroll()
                    .track_scroll(&self.profile_scroll)
                    .children(rows)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .py_3()
                                .text_center()
                                .text_size(px(12.))
                                .text_color(p.dim)
                                .child("No matching profiles"),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not a glob: `gpui_kit::*` would shadow the `#[test]` attribute.
    use super::{profile_items, search};
    use crate::store::{DEFAULT_PROFILE, Profile};

    fn profile(id: &str, name: &str, description: &str) -> Profile {
        Profile {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            tags: vec![],
            kind: String::new(),
            source: String::new(),
        }
    }

    fn names(query: &str) -> Vec<String> {
        let items = profile_items(&[
            profile("cdr", "Coder Delegate", "Writes code"),
            profile("def-qmcp", "def-qmcp", "QueryMT MCP tools"),
            profile("speech", "Default (Speech)", "Voice conversation"),
        ]);
        search(items, query).into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn search_matches_name_id_and_description() {
        assert_eq!(names("").len(), 4);
        assert_eq!(names("")[0], "Default profile");
        assert_eq!(names("cdr"), ["Coder Delegate"]);
        assert_eq!(names("voice"), ["Default (Speech)"]);
        assert_eq!(names("qmcp").first().map(String::as_str), Some("def-qmcp"));
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn no_profiles_means_no_picker_items() {
        assert!(profile_items(&[]).is_empty());
        assert_eq!(
            profile_items(&[profile("a", "A", "")])[0].id,
            DEFAULT_PROFILE
        );
    }
}
