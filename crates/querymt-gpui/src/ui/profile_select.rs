//! Searchable selects of the Start composer's Options (Profile, Target), in
//! the app's select style (trigger like the settings pill selects, menu like
//! the session search): the search fuzzy-matches name, id and description,
//! so long lists (dozens of mesh hosts) stay quick to pick from.

use gpui_kit::assets::IconName;
use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner};
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{QmtApp, kit};
use crate::store::launch::OWNER_DEFAULT_PROFILE;
use crate::store::{DEFAULT_PROFILE, Profile};
use crate::theme::Palette;

#[derive(Clone, Debug, PartialEq)]
pub struct ProfileItem {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Leading icon of the row (targets: this machine or a mesh host).
    pub icon: Option<IconName>,
}

/// A select of the Options popover.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OptionMenu {
    Profile,
    Target,
}

impl OptionMenu {
    /// Popover key (see [`kit::Dismissed`]).
    fn key(self) -> &'static str {
        match self {
            Self::Profile => "profile-menu",
            Self::Target => "target-menu",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Profile => "Search by name, ID or description",
            Self::Target => "Search hosts by name or ID",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Self::Profile => "No matching profiles",
            Self::Target => "No matching hosts",
        }
    }

    /// Mono tag at the end of a row: a profile's id, or the tail of a host's
    /// peer id (enough to tell hosts with the same name apart). The search
    /// still matches the whole id.
    fn tag(self, id: &str) -> Option<String> {
        match self {
            Self::Profile => Some(id.to_owned()),
            Self::Target if id == LOCAL_TARGET => None,
            Self::Target => Some(peer_tag(id)),
        }
    }
}

/// The last characters of a peer id: its leading characters are often a
/// shared multihash prefix, the tail is what differs.
fn peer_tag(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 10 {
        id.to_owned()
    } else {
        format!("…{}", chars[chars.len() - 8..].iter().collect::<String>())
    }
}

/// Id of "Local" among the targets.
const LOCAL_TARGET: &str = "local";

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
        icon: None,
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
                    icon: None,
                }),
        )
        .collect()
}

/// "Owner default" first, then the mesh peer's own profiles.
fn peer_profile_items(profiles: &[Profile]) -> Vec<ProfileItem> {
    let default = ProfileItem {
        id: OWNER_DEFAULT_PROFILE.into(),
        name: "Owner default".into(),
        description: "Use the peer's active profile.".into(),
        icon: None,
    };
    std::iter::once(default)
        .chain(profiles.iter().map(|p| ProfileItem {
            id: p.id.clone(),
            name: p.name.clone(),
            description: p.description.clone(),
            icon: None,
        }))
        .collect()
}

/// Items matching `query`, best first; equal scores keep the list order.
fn search(items: Vec<ProfileItem>, query: &str) -> Vec<ProfileItem> {
    let ranked = crate::search::rank_tiered(
        query,
        &items,
        |p| vec![p.name.clone()],
        |p| vec![p.id.clone(), p.description.clone()],
    );
    let mut slots: Vec<Option<ProfileItem>> = items.into_iter().map(Some).collect();
    ranked
        .into_iter()
        .filter_map(|(i, _)| slots[i].take())
        .collect()
}

/// Gap between the menu and the window edge.
const MENU_MARGIN: f32 = 8.;
/// Bounds of the scrolling row list (about 3 to 9 rows).
const MIN_LIST_H: f32 = 96.;
const MAX_LIST_H: f32 = 280.;

/// What a select shows.
struct Choices {
    agent: String,
    /// Profile select for a mesh peer: its profiles are listed.
    peer: Option<String>,
    selected: String,
    items: Vec<ProfileItem>,
}

impl QmtApp {
    fn choices(&self, menu: OptionMenu, cx: &App) -> Option<Choices> {
        match menu {
            OptionMenu::Profile => self.profile_choices(cx),
            OptionMenu::Target => self.target_choices(cx),
        }
    }

    fn profile_choices(&self, cx: &App) -> Option<Choices> {
        let store = self.store.read(cx);
        let agent = store.primary_agent()?;
        if let Some(peer) = store.launch.target.clone() {
            let profiles = store
                .peer_profiles(&agent, &peer)
                .map(|p| p.profiles.as_slice())
                .unwrap_or_default();
            return Some(Choices {
                selected: store
                    .launch
                    .remote_profile
                    .clone()
                    .unwrap_or_else(|| OWNER_DEFAULT_PROFILE.into()),
                items: peer_profile_items(profiles),
                agent,
                peer: Some(peer),
            });
        }
        let items = profile_items(store.profiles(&agent));
        (!items.is_empty()).then(|| Choices {
            selected: store.launch_profile(&agent).to_string(),
            agent,
            peer: None,
            items,
        })
    }

    /// Local, then the agent's mesh hosts; only when there is a host.
    fn target_choices(&self, cx: &App) -> Option<Choices> {
        let store = self.store.read(cx);
        let agent = store.primary_agent()?;
        let targets = store.launch_targets(&agent);
        if targets.len() < 2 {
            return None;
        }
        let items = targets
            .into_iter()
            .map(|t| ProfileItem {
                id: t.node_id.clone().unwrap_or_else(|| LOCAL_TARGET.into()),
                name: t.label,
                description: if t.unavailable {
                    "Unavailable · not reachable on the mesh".into()
                } else {
                    t.detail
                },
                icon: Some(if t.node_id.is_some() {
                    IconName::Network
                } else {
                    IconName::Monitor
                }),
            })
            .collect();
        Some(Choices {
            selected: store
                .launch
                .target
                .clone()
                .unwrap_or_else(|| LOCAL_TARGET.into()),
            agent,
            peer: None,
            items,
        })
    }

    /// Loading or error line of a mesh peer's profiles, under the picker.
    pub(super) fn peer_profiles_note(&self, p: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let store = self.store.read(cx);
        let agent = store.primary_agent()?;
        let peer = store.launch.target.clone()?;
        let state = store.peer_profiles(&agent, &peer)?;
        if state.loading {
            return Some(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(p.muted)
                    .child(kit::spinner("peer-profiles-spin", 12., p.muted))
                    .child("Loading the peer's profiles…"),
            );
        }
        let error = state.error.clone()?;
        Some(
            h_flex()
                .gap_1p5()
                .text_xs()
                .text_color(p.danger)
                .child(kit::icon(IconName::CircleAlert, 12., p.danger))
                .child(div().flex_1().min_w_0().child(error))
                .child(
                    kit::flat_btn("peer-profiles-retry", true, true, p)
                        .h(px(26.))
                        .child("Retry")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |s, cx| s.load_peer_profiles(&agent, &peer, cx))
                        })),
                ),
        )
    }

    /// Items of the open select matching its search query.
    fn matched_items(&self, cx: &App) -> Vec<ProfileItem> {
        let Some(menu) = self.option_menu else {
            return Vec::new();
        };
        let query = self.profile_query.read(cx).value().to_string();
        self.choices(menu, cx)
            .map_or_else(Vec::new, |c| search(c.items, &query))
    }

    pub(super) fn open_option_menu(
        &mut self,
        menu: OptionMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.choices(menu, cx).map(|c| c.selected);
        self.option_menu = Some(menu);
        self.profile_query.update(cx, |s, cx| {
            s.set_value(SharedString::default(), window, cx);
            s.set_placeholder(menu.placeholder(), window, cx);
            s.focus(window, cx);
        });
        self.profile_cursor = self
            .matched_items(cx)
            .iter()
            .position(|p| Some(&p.id) == selected.as_ref())
            .unwrap_or(0);
        self.profile_scroll.scroll_to_item(self.profile_cursor);
        cx.notify();
    }

    pub(super) fn close_option_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.option_menu.take().is_some() {
            self.app_focus.focus(window, cx);
            cx.notify();
        }
    }

    fn pick_item(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.option_menu else { return };
        match (menu, self.choices(menu, cx)) {
            (OptionMenu::Profile, Some(Choices { peer: Some(_), .. })) => {
                let id = id.to_owned();
                self.store
                    .update(cx, |s, cx| s.set_launch_remote_profile(Some(id), cx));
            }
            (OptionMenu::Profile, Some(Choices { agent, .. })) => {
                self.store
                    .update(cx, |s, cx| s.set_launch_profile(&agent, id, cx));
            }
            (OptionMenu::Target, Some(Choices { agent, .. })) => {
                let node = (id != LOCAL_TARGET).then(|| id.to_owned());
                self.store
                    .update(cx, |s, cx| s.set_launch_target(&agent, node, cx));
                // A peer's paths are completed from its sessions.
                self.workspace.update(cx, |_, cx| cx.notify());
            }
            (_, None) => {}
        }
        self.close_option_menu(window, cx);
    }

    /// Enter in the search box picks the highlighted row.
    pub(super) fn pick_option_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.matched_items(cx).get(self.profile_cursor) {
            let id = item.id.clone();
            self.pick_item(&id, window, cx);
        }
    }

    fn move_option_cursor(&mut self, down: bool, cx: &mut Context<Self>) {
        let len = self.matched_items(cx).len();
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
        self.option_select(OptionMenu::Profile, p, cx)
    }

    /// The Target row of the Options popover, when the agent has mesh hosts.
    pub(super) fn target_picker(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.option_select(OptionMenu::Target, p, cx)
    }

    fn option_select(
        &self,
        menu: OptionMenu,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let choices = self.choices(menu, cx)?;
        let open = self.option_menu == Some(menu);
        let current = choices.items.iter().find(|i| i.id == choices.selected);
        let label = current.map_or_else(|| choices.selected.clone(), |i| i.name.clone());
        let icon = current.and_then(|i| i.icon).unwrap_or(IconName::FileCog);
        // Same trigger as the settings pill selects (auth method).
        let trigger = kit::action_btn(
            match menu {
                OptionMenu::Profile => "opt-profile",
                OptionMenu::Target => "opt-target",
            },
            true,
            true,
            p,
        )
        .w_full()
        .justify_start()
        .when(open, |el| el.border_color(p.border_strong))
        .child(kit::icon(icon, 14., p.muted))
        .child(div().flex_1().min_w_0().truncate().child(label))
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
            if this.dismissed.closed_by(menu.key(), ev) {
                return;
            }
            if this.option_menu == Some(menu) {
                this.close_option_menu(window, cx);
            } else {
                this.open_option_menu(menu, window, cx);
            }
        }));

        // Below the trigger, flipped above it when the window is too short,
        // and kept inside the window either way.
        let trigger_bounds = self.option_triggers[menu as usize];
        let list = open.then(|| {
            let positioner = Positioner::side(trigger_bounds)
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(4.))
                .margin(px(MENU_MARGIN))
                .on_position({
                    let bounds = self.profile_menu_bounds.clone();
                    move |pos| bounds.set(pos.bounds)
                });
            deferred(positioner.child(self.render_option_menu(menu, &choices.selected, p, cx)))
                .with_priority(2)
        });
        let this = cx.entity().downgrade();
        Some(
            div()
                .w_full()
                .child(trigger)
                .children(list)
                .on_prepaint(move |bounds, window, cx| {
                    let viewport = window.viewport_size().height;
                    let _ = this.update(cx, |this, cx| {
                        let slot = &mut this.option_triggers[menu as usize];
                        if *slot != bounds || this.profile_viewport_h != viewport {
                            *slot = bounds;
                            this.profile_viewport_h = viewport;
                            if this.option_menu == Some(menu) {
                                cx.notify();
                            }
                        }
                    });
                })
                .into_any_element(),
        )
    }

    /// Row-list height that lets the menu fit on the roomier side of the trigger.
    fn option_list_max_h(&self, menu: OptionMenu) -> Pixels {
        let trigger = self.option_triggers[menu as usize];
        let below = self.profile_viewport_h - trigger.bottom();
        let above = trigger.top();
        // Search box, paddings, gap, offset, window margin and frame inset.
        let chrome = px(32. + 8. + 4. + 2. + 4. + MENU_MARGIN + 16.);
        (below.max(above) - chrome).clamp(px(MIN_LIST_H), px(MAX_LIST_H))
    }

    fn render_option_menu(
        &self,
        menu: OptionMenu,
        selected: &str,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hover = p.card_hover;
        let rows: Vec<AnyElement> = self
            .matched_items(cx)
            .into_iter()
            .enumerate()
            .map(|(ix, item)| {
                let checked = item.id == selected;
                let id = item.id.clone();
                h_flex()
                    .id(kit::keyed(
                        match menu {
                            OptionMenu::Profile => "opt-profile-row",
                            OptionMenu::Target => "opt-target-row",
                        },
                        item.id.clone(),
                    ))
                    .flex_none()
                    .h(px(30.))
                    .px_2()
                    .gap_3()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(kit::rpx(12.))
                    .hover(move |s| s.bg(hover))
                    .when(ix == self.profile_cursor, |el| el.bg(hover))
                    .children(item.icon.map(|icon| kit::icon(icon, 13., p.dim)))
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
                            .text_size(kit::rpx(11.2))
                            .text_color(p.dim)
                            .child(item.description),
                    )
                    .children(menu.tag(&item.id).map(|tag| {
                        div()
                            .flex_none()
                            .max_w(px(110.))
                            .truncate()
                            .font_family(kit::mono_family(cx))
                            .text_size(kit::rpx(11.))
                            .text_color(p.dim)
                            .child(tag)
                    }))
                    .child(
                        kit::icon(IconName::Check, 13., p.accent)
                            .flex_none()
                            .when(!checked, |el| el.invisible()),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.pick_item(&id, window, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();

        v_flex()
            .id(("opt-menu", menu as usize))
            .w(px(460.))
            .p_1()
            .gap_1()
            .rounded(px(10.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                this.dismissed.record(menu.key(), ev);
                this.close_option_menu(window, cx);
            }))
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "up" => this.move_option_cursor(false, cx),
                    "down" => this.move_option_cursor(true, cx),
                    "escape" => this.close_option_menu(window, cx),
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
                    .id(("opt-list", menu as usize))
                    .max_h(self.option_list_max_h(menu))
                    .overflow_y_scroll()
                    .track_scroll(&self.profile_scroll)
                    .children(rows)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .py_3()
                                .text_center()
                                .text_size(kit::rpx(12.))
                                .text_color(p.dim)
                                .child(menu.empty()),
                        )
                    })
                    .map(|el| self.smooth_scroll.region("option-list", el)),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not a glob: `gpui_kit::*` would shadow the `#[test]` attribute.
    use super::{OptionMenu, ProfileItem, peer_tag, profile_items, search};

    #[test]
    fn host_rows_end_with_the_peer_id_tail() {
        assert_eq!(peer_tag("12D3KooWAbCdEfGh12345678"), "…12345678");
        assert_eq!(peer_tag("short"), "short");
        assert_eq!(OptionMenu::Target.tag("local"), None);
        assert_eq!(OptionMenu::Profile.tag("dev").as_deref(), Some("dev"));
    }

    #[test]
    fn hosts_match_fuzzily_by_name_and_any_part_of_the_peer_id() {
        let host = |id: &str, name: &str| ProfileItem {
            id: id.into(),
            name: name.into(),
            description: "No active sessions".into(),
            icon: None,
        };
        let hosts = || {
            vec![
                host("local", "Local"),
                host("12D3KooWThorAAAA9f3e01bc", "thor"),
                host("12D3KooWFlkrBBBB77aa42de", "flkr"),
                host("12D3KooWPhoneCCCC5c5c0011", "Nothing phone (1)"),
            ]
        };
        let names =
            |q: &str| -> Vec<String> { search(hosts(), q).into_iter().map(|h| h.name).collect() };
        // Name, fuzzily: letters in order, gaps allowed.
        assert_eq!(names("thr"), ["thor"]);
        assert_eq!(names("nth phn"), ["Nothing phone (1)"]);
        // Any part of the peer id: the tail shown in the row, or the middle.
        assert_eq!(names("77aa42de"), ["flkr"]);
        assert_eq!(names("9f3e"), ["thor"]);
        assert_eq!(names("cccc5c"), ["Nothing phone (1)"]);
    }
    use crate::store::{DEFAULT_PROFILE, Profile};

    fn profile(id: &str, name: &str, description: &str) -> Profile {
        Profile {
            id: id.into(),
            name: name.into(),
            description: description.into(),
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
