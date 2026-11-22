//! The sidebar's session lists as tabs (desktop `RecentSessionRail`, plus
//! what browsers do): a stable order the user arranges by dragging, a right
//! click menu with what can be done to a session, a dot on sessions waiting
//! for the user, and the open list growing as it is scrolled.

use gpui_kit::assets::IconName;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Page, QmtApp, kit};
use crate::store::SessionSummary;
use crate::store::sidebar::{SideList, tab_key};
use crate::theme::{Palette, palette};

/// Open sessions shown at first and added per page as the list is scrolled
/// (desktop `SESSION_PAGE_SIZE`).
pub(super) const OPEN_PAGE: usize = 20;
/// Pinned sessions shown at first and added per "Show more pinned".
pub(super) const PINNED_PAGE: usize = 5;
/// Sessions on the collapsed rail.
pub(super) const RAIL_SESSIONS: usize = 10;
/// How close to the bottom the open list loads its next page.
const LOAD_MARGIN: f32 = 120.;

const COPIED_SIDE_PATH: &str = "side-menu-path";
const COPIED_SIDE_ID: &str = "side-menu-id";

/// Where a dragged row would land: before or after the row under the
/// pointer, by which half of it the pointer is in (like browser tabs).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DropTarget {
    list: SideList,
    key: String,
    after: bool,
}

/// Width of a sidebar row: the sidebar less its 9px + 8px gutters.
const ROW_W: f32 = super::SIDEBAR_W - 17.;

/// What a dragged row looks like: a copy of the row, so it reads as the
/// row itself moving (browser tabs), not as a tooltip.
#[derive(Clone)]
pub(super) enum TabLook {
    /// A tile of the collapsed rail.
    Tile,
    /// A pinned row: icon and title.
    Pinned,
    /// An open-session row: title, then icon, workspace and time.
    Open {
        workspace: SharedString,
        time: SharedString,
    },
}

/// The right-click menu of one session.
pub(super) struct SideMenu {
    pub(super) session: SessionSummary,
    at: Point<Pixels>,
}

pub(super) struct SideUi {
    pub(super) open_limit: usize,
    pub(super) pinned_limit: usize,
    pub(super) scroll: ScrollHandle,
    pub(super) drop_target: Option<DropTarget>,
    pub(super) menu: Option<SideMenu>,
}

impl Default for SideUi {
    fn default() -> Self {
        Self {
            open_limit: OPEN_PAGE,
            pinned_limit: PINNED_PAGE,
            scroll: ScrollHandle::new(),
            drop_target: None,
            menu: None,
        }
    }
}

/// A row being dragged, and its picture under the pointer.
#[derive(Clone)]
pub(super) struct DraggedTab {
    list: SideList,
    key: String,
    ix: usize,
    seed: String,
    title: SharedString,
    look: TabLook,
    p: Palette,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let p = self.p;
        // Same size and layout as the row, so it stays under the pointer
        // where it was grabbed; see-through, lifted by a shadow.
        let shell = div()
            .rounded(px(8.))
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            .shadow_md()
            .opacity(0.88);
        let title = div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(self.title.clone());
        let shell = shell.debug_selector(|| "tab-drag-preview".into());
        match &self.look {
            TabLook::Tile => shell
                .size(kit::RAIL_TILE)
                .flex()
                .items_center()
                .justify_center()
                .child(kit::identicon(&self.seed, 30.))
                .into_any_element(),
            TabLook::Pinned => shell
                .w(px(ROW_W))
                .h(px(28.))
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(p.text)
                .child(kit::identicon(&self.seed, 14.))
                .child(title)
                .into_any_element(),
            TabLook::Open { workspace, time } => shell
                .w(px(ROW_W))
                .px_2()
                .py_1p5()
                .flex()
                .flex_col()
                .gap_0p5()
                .text_sm()
                .text_color(p.text)
                .child(h_flex().child(title))
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(p.dim)
                        .child(kit::identicon(&self.seed, 14.))
                        .child(div().flex_1().min_w_0().truncate().child(workspace.clone()))
                        .child(div().flex_none().child(time.clone())),
                )
                .into_any_element(),
        }
    }
}

/// What the sidebar shows.
pub(super) struct Shown {
    pub(super) pinned: Vec<SessionSummary>,
    pub(super) pinned_total: usize,
    pub(super) open: Vec<SessionSummary>,
    pub(super) open_total: usize,
}

/// The first `limit` of `list`, with the current session in place of the
/// last one when it would not be shown (desktop rail).
fn take_keeping(
    mut list: Vec<&SessionSummary>,
    limit: usize,
    current: Option<&str>,
) -> Vec<SessionSummary> {
    if list.len() <= limit {
        return list.into_iter().cloned().collect();
    }
    let current_ix = current.and_then(|key| {
        let key = key.split_once('/')?;
        list.iter()
            .position(|s| (s.agent_id.as_str(), s.id.as_str()) == key)
    });
    let keep = current_ix
        .filter(|ix| *ix >= limit)
        .map(|ix| list.remove(ix));
    list.truncate(limit);
    if let Some(s) = keep
        && limit > 0
    {
        list[limit - 1] = s;
    }
    list.into_iter().cloned().collect()
}

impl QmtApp {
    /// `agent/session` of the session open in the transcript.
    fn current_tab(&self, cx: &App) -> Option<String> {
        (self.page == Page::Session)
            .then(|| self.store.read(cx).session.as_ref())
            .flatten()
            .map(|v| format!("{}/{}", v.agent_id, v.session_id))
    }

    /// The sidebar's sessions as shown: expanded, pinned then open within
    /// their limits; collapsed, the rail's first few of both.
    pub(super) fn sidebar_shown(&self, cx: &App) -> Shown {
        let store = self.store.read(cx);
        let current = self.current_tab(cx);
        let pinned = store.sidebar_sessions(SideList::Pinned);
        let open = store.sidebar_sessions(SideList::Open);
        let (pinned_total, open_total) = (pinned.len(), open.len());
        if store.prefs.sidebar_collapsed {
            let all: Vec<&SessionSummary> = pinned.into_iter().chain(open).collect();
            return Shown {
                pinned: Vec::new(),
                pinned_total,
                open: take_keeping(all, RAIL_SESSIONS, current.as_deref()),
                open_total,
            };
        }
        Shown {
            pinned: take_keeping(pinned, self.side.pinned_limit, current.as_deref()),
            pinned_total,
            open: take_keeping(open, self.side.open_limit, current.as_deref()),
            open_total,
        }
    }

    /// Show the next page of open sessions once the list is scrolled near
    /// its end (desktop `loadNearBottom`).
    pub(super) fn sidebar_near_bottom(&mut self, cx: &mut Context<Self>) {
        let (offset, max) = (self.side.scroll.offset(), self.side.scroll.max_offset());
        if max.y + offset.y > px(LOAD_MARGIN) {
            return;
        }
        let total = self.store.read(cx).sidebar_sessions(SideList::Open).len();
        if self.side.open_limit < total {
            self.side.open_limit += OPEN_PAGE;
            cx.notify();
        }
    }

    /// Make `row` a tab of `list`: draggable within it, a drop target
    /// showing where the dragged row would land, and a right-click menu.
    /// `gap`: space between this list's rows; the drop line sits in it.
    /// `len`: rows in the list, so the line at either end stays inside it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tab_row(
        &self,
        row: Stateful<Div>,
        list: SideList,
        ix: usize,
        len: usize,
        s: &SessionSummary,
        look: TabLook,
        gap: f32,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let key = tab_key(s);
        let marker = self
            .side
            .drop_target
            .as_ref()
            .filter(|t| t.list == list && t.key == key)
            .map(|t| t.after);
        let dragged = DraggedTab {
            list,
            key: key.clone(),
            ix,
            seed: s.id.clone(),
            title: s.display_title().into(),
            look,
            p: *p,
        };
        let k_move = key.clone();
        let menu_s = s.clone();
        let selector = format!("tab-{key}");
        row.relative()
            .debug_selector(move || selector.clone())
            .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
            .on_drag_move(
                cx.listener(move |this, ev: &DragMoveEvent<DraggedTab>, _, cx| {
                    // Every row hears every move: the one under the pointer
                    // places the line; the others clear their own.
                    let target = if ev.bounds.contains(&ev.event.position) {
                        let d = ev.drag(cx);
                        let after = ev.event.position.y > ev.bounds.center().y;
                        // Next to itself, the row would not move: no line.
                        let stays = d.key == k_move
                            || (after && ix + 1 == d.ix)
                            || (!after && d.ix + 1 == ix);
                        (d.list == list && !stays).then(|| DropTarget {
                            list,
                            key: k_move.clone(),
                            after,
                        })
                    } else if this
                        .side
                        .drop_target
                        .as_ref()
                        .is_some_and(|t| t.list == list && t.key == k_move)
                    {
                        None
                    } else {
                        return;
                    };
                    if this.side.drop_target != target {
                        this.side.drop_target = target;
                        cx.notify();
                    }
                }),
            )
            // The drop goes where the line shows.
            .on_drop(cx.listener(move |this, d: &DraggedTab, _, cx| {
                if let Some(t) = this.side.drop_target.take()
                    && t.list == d.list
                {
                    let from = d.key.clone();
                    this.store.update(cx, |s, cx| {
                        s.move_sidebar_session(t.list, &from, &t.key, t.after, cx)
                    });
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.session_menu_open = false;
                    this.side.menu = Some(SideMenu {
                        session: menu_s.clone(),
                        at: ev.position,
                    });
                    cx.notify();
                }),
            )
            .when_some(marker, |el, after| {
                // At the ends the gap may be the scroll edge, which would
                // clip the line: it goes just inside the row instead.
                let end = if after { ix + 1 == len } else { ix == 0 };
                el.child(drop_line(after, if end { None } else { Some(gap) }, p))
            })
    }

    /// The right-click menu of a sidebar session (the actions of the
    /// session's own menu, and Delete).
    pub(super) fn render_side_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.side.menu.as_ref()?;
        let p = *palette(cx);
        let s = menu.session.clone();
        let (pinned, resolved, archived) = (s.pinned, s.resolved, s.archived);
        let (can_pin, can_lifecycle, can_delete) = {
            let rt = self.store.read(cx).runtime(&s.agent_id);
            let local = s.remote.is_none();
            (
                rt.is_some_and(|r| r.can_pin()) && s.root && local,
                rt.is_some_and(|r| r.can_set_lifecycle())
                    && s.lifecycle_known
                    && s.remote.as_ref().is_none_or(|r| r.connected),
                rt.is_some_and(|r| r.is_connected() && r.can_delete_sessions) && local,
            )
        };
        let item = |id: &'static str, icon: IconName, label: &str| {
            kit::menu_item(id, icon, label.to_string(), &p).debug_selector(move || id.into())
        };
        let (a1, s1) = (s.agent_id.clone(), s.id.clone());
        let (a2, s2) = (s.agent_id.clone(), s.id.clone());
        let (a3, s3) = (s.agent_id.clone(), s.id.clone());
        let (cwd, sid) = (s.cwd.clone(), s.id.clone());
        let path_done = self.copied.as_deref() == Some(COPIED_SIDE_PATH);
        let id_done = self.copied.as_deref() == Some(COPIED_SIDE_ID);
        let rule = || div().my_1().child(kit::divider(&p));
        let panel = v_flex()
            .id("side-menu")
            .w(px(220.))
            .p_1()
            .rounded(px(12.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.side.menu = None;
                cx.notify();
            }))
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_1p5()
                    .text_xs()
                    .text_color(p.dim)
                    .truncate()
                    .child(s.display_title()),
            )
            .when(can_pin, |el| {
                el.child(
                    item(
                        "side-pin",
                        if pinned {
                            IconName::PinOff
                        } else {
                            IconName::Pin
                        },
                        if pinned { "Unpin" } else { "Pin" },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.side.menu = None;
                        this.store
                            .update(cx, |st, cx| st.set_pinned(&a1, &s1, !pinned, cx));
                    })),
                )
            })
            .when(can_lifecycle, |el| {
                el.child(
                    item(
                        "side-resolve",
                        if resolved {
                            IconName::RotateCcw
                        } else {
                            IconName::CircleCheck
                        },
                        if resolved { "Reopen" } else { "Mark resolved" },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.side.menu = None;
                        this.store.update(cx, |st, cx| {
                            st.set_lifecycle(&a2, &s2, Some(!resolved), None, cx)
                        });
                    })),
                )
                .child(
                    item(
                        "side-archive",
                        if archived {
                            IconName::ArchiveRestore
                        } else {
                            IconName::Archive
                        },
                        if archived { "Unarchive" } else { "Archive" },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.side.menu = None;
                        this.store.update(cx, |st, cx| {
                            st.set_lifecycle(&a3, &s3, None, Some(!archived), cx)
                        });
                    })),
                )
            })
            .when(can_pin || can_lifecycle, |el| el.child(rule()))
            // Like the session menu: copy items stay open and read "Copied".
            .when(!cwd.is_empty(), |el| {
                el.child(
                    item(
                        "side-copy-path",
                        if path_done {
                            IconName::Check
                        } else {
                            IconName::FolderOpen
                        },
                        if path_done {
                            "Copied"
                        } else {
                            "Copy project path"
                        },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(cwd.clone()));
                        this.flash_copied(COPIED_SIDE_PATH.into(), cx);
                    })),
                )
            })
            .child(
                item(
                    "side-copy-id",
                    if id_done {
                        IconName::Check
                    } else {
                        IconName::FingerprintPattern
                    },
                    if id_done { "Copied" } else { "Copy session ID" },
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sid.clone()));
                    this.flash_copied(COPIED_SIDE_ID.into(), cx);
                })),
            )
            .when(can_delete, |el| {
                let s = s.clone();
                el.child(rule()).child(
                    danger_item("side-delete", IconName::Trash, "Delete session…", &p).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.side.menu = None;
                            this.confirm_session_delete = Some(s.clone());
                            cx.notify();
                        }),
                    ),
                )
            });
        Some(
            deferred(
                anchored()
                    .position(menu.at)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }
}

/// Where the dragged row will go: a line in the gap between two rows, with
/// a ring at its start (browser tab strips). Without a `gap`, along the
/// row's own edge.
fn drop_line(after: bool, gap: Option<f32>, p: &Palette) -> Div {
    const H: f32 = 8.;
    let edge = px(gap.map_or(0., |gap| -(gap / 2.) - H / 2.));
    let color = p.muted;
    h_flex()
        .debug_selector(|| "tab-drop-line".into())
        .absolute()
        .left_0()
        .right_0()
        .h(px(H))
        .map(|m| if after { m.bottom(edge) } else { m.top(edge) })
        .child(
            div()
                .flex_none()
                .size(px(H))
                .rounded_full()
                .border(px(1.5))
                .border_color(color)
                .bg(p.bg),
        )
        .child(div().flex_1().h(px(1.5)).bg(color))
}

/// [`kit::menu_item`] in the destructive tone.
fn danger_item(
    id: &'static str,
    name: IconName,
    label: &'static str,
    p: &Palette,
) -> Stateful<Div> {
    let hover = p.danger.opacity(0.1);
    h_flex()
        .id(id)
        .debug_selector(move || id.into())
        .w_full()
        .h(px(28.))
        .px_2()
        .gap_2()
        .rounded(px(6.))
        .cursor_pointer()
        .text_sm()
        .text_color(p.danger)
        .hover(move |s| s.bg(hover))
        .child(kit::icon(name, 14., p.danger))
        .child(div().min_w_0().truncate().child(label))
}

/// Dot on a session waiting for the user (a permission or a question).
pub(super) fn waiting_dot(session_id: &str, p: &Palette) -> Stateful<Div> {
    let selector = format!("wait-{session_id}");
    div()
        .id(kit::keyed("wait", session_id.to_string()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .size(px(7.))
        .rounded_full()
        .bg(p.accent)
        .tooltip(|window, cx| kit::wrap_tip("Waiting for you").build(window, cx))
}

#[cfg(test)]
mod tests {
    use super::take_keeping;
    use crate::store::SessionSummary;

    fn s(id: &str) -> SessionSummary {
        SessionSummary {
            agent_id: "a".into(),
            id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_current_session_stays_in_view() {
        let owned: Vec<SessionSummary> = ["1", "2", "3", "4"].map(s).to_vec();
        let list: Vec<&SessionSummary> = owned.iter().collect();
        let ids = |v: Vec<SessionSummary>| v.into_iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids(take_keeping(list.clone(), 2, None)), ["1", "2"]);
        assert_eq!(ids(take_keeping(list.clone(), 2, Some("a/4"))), ["1", "4"]);
        assert_eq!(ids(take_keeping(list, 2, Some("a/1"))), ["1", "2"]);
    }
}

#[cfg(test)]
mod ui_tests {
    use crate::store::{ConnState, SessionSummary};
    use crate::ui::QmtApp;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Modifiers, MouseButton, ScrollDelta, ScrollEvent,
        TestAppContext, VisualTestContext, point, px,
    };
    use serde_json::{Value, json};

    fn session(id: &str, updated: &str) -> SessionSummary {
        SessionSummary {
            agent_id: "test-agent".into(),
            id: id.into(),
            title: format!("Session {id}"),
            cwd: "/w".into(),
            updated_at: updated.into(),
            root: true,
            open: true,
            lifecycle_known: true,
            ..Default::default()
        }
    }

    /// `n` open sessions, `s1` the most recent; the agent settled.
    fn app(
        n: usize,
        cx: &mut TestAppContext,
    ) -> (
        AnyWindowHandle,
        gpui_kit::Entity<QmtApp>,
        std::rc::Rc<crate::rpc::RpcClient>,
        async_channel::Receiver<String>,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.sessions_loaded = true;
        rt.can_delete_sessions = true;
        rt.capabilities = Some(json!({
            "methods": ["querymt/session/setPinned", "querymt/session/setLifecycle"],
            "features": { "session_lifecycle": true }
        }));
        rt.sessions = (1..=n)
            .map(|i| {
                session(
                    &format!("s{i}"),
                    &format!("2026-01-01T00:{:02}:00Z", 60 - i),
                )
            })
            .collect();
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        render(handle, cx);
        (handle, app, rpc, requests)
    }

    fn render(handle: AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn shown_ids(app: &gpui_kit::Entity<QmtApp>, cx: &mut TestAppContext) -> Vec<String> {
        app.read_with(cx, |app, cx| {
            app.sidebar_shown(cx)
                .open
                .into_iter()
                .map(|s| s.id)
                .collect()
        })
    }

    fn sent(requests: &async_channel::Receiver<String>) -> Vec<Value> {
        std::iter::from_fn(|| requests.try_recv().ok())
            .map(|r| serde_json::from_str(&r).unwrap())
            .collect()
    }

    #[gpui_kit::test]
    fn right_click_offers_the_session_actions(cx: &mut TestAppContext) {
        let (handle, app, _rpc, requests) = app(3, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let row = vcx.debug_bounds("tab-test-agent/s2").expect("row");
        vcx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
        vcx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
        render(handle, cx);
        app.read_with(cx, |app, _| {
            assert_eq!(app.side.menu.as_ref().unwrap().session.id, "s2");
        });

        // Pin asks the agent; the menu closes.
        let mut vcx = VisualTestContext::from_window(handle, cx);
        for id in [
            "side-resolve",
            "side-archive",
            "side-copy-path",
            "side-copy-id",
        ] {
            assert!(vcx.debug_bounds(id).is_some(), "{id}");
        }
        let pin = vcx.debug_bounds("side-pin").expect("Pin");
        vcx.simulate_click(pin.center(), Modifiers::none());
        assert!(
            sent(&requests)
                .iter()
                .any(|r| r["method"] == "_querymt/session/setPinned"
                    && r["params"]["pinned"] == true),
        );
        app.read_with(cx, |app, _| assert!(app.side.menu.is_none()));

        // Delete asks first.
        render(handle, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let row = vcx.debug_bounds("tab-test-agent/s3").expect("row");
        vcx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
        render(handle, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let delete = vcx.debug_bounds("side-delete").expect("Delete");
        vcx.simulate_click(delete.center(), Modifiers::none());
        app.read_with(cx, |app, _| {
            assert_eq!(app.confirm_session_delete.as_ref().unwrap().id, "s3");
        });
    }

    #[gpui_kit::test]
    fn rows_keep_their_place_and_move_by_dragging(cx: &mut TestAppContext) {
        let (handle, app, _rpc, _requests) = app(4, cx);
        assert_eq!(shown_ids(&app, cx), ["s1", "s2", "s3", "s4"]);

        // Activity does not reorder; a new session goes on top.
        let store = app.read_with(cx, |app, _| app.store.clone());
        cx.update(|cx| {
            store.update(cx, |s, _| {
                let rt = s.runtime_mut_for_test("test-agent");
                rt.sessions[3].updated_at = "2026-02-01T00:00:00Z".into();
                rt.sessions.push(session("s5", "2026-01-01T00:00:00Z"));
            })
        });
        render(handle, cx);
        assert_eq!(shown_ids(&app, cx), ["s5", "s1", "s2", "s3", "s4"]);

        // Drag s5 (top) down.
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let from = vcx.debug_bounds("tab-test-agent/s5").unwrap().center();
        let s1 = vcx.debug_bounds("tab-test-agent/s1").unwrap();
        let s3 = vcx.debug_bounds("tab-test-agent/s3").unwrap();
        vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        vcx.simulate_mouse_move(
            point(from.x, from.y + px(10.)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        // Just above s1 is where s5 already is: no line.
        let above_s1 = point(s1.center().x, s1.center().y - px(4.));
        vcx.simulate_mouse_move(above_s1, Some(MouseButton::Left), Modifiers::none());
        app.read_with(cx, |app, _| assert!(app.side.drop_target.is_none()));
        // The lower half of s3: after it.
        let to = point(s3.center().x, s3.center().y + px(4.));
        vcx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        app.read_with(cx, |app, _| {
            let t = app.side.drop_target.as_ref().expect("drop line");
            assert!(t.key.ends_with("/s3") && t.after);
        });
        // The dragged picture is the row, at its size.
        render(handle, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let preview = vcx.debug_bounds("tab-drag-preview").expect("drag picture");
        assert!((preview.size.width - s3.size.width).abs() < px(2.));
        vcx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        render(handle, cx);
        assert_eq!(shown_ids(&app, cx), ["s1", "s2", "s3", "s5", "s4"]);
        store.read_with(cx, |s, _| {
            assert_eq!(
                s.prefs.sidebar_open_order[..2],
                ["test-agent/s1", "test-agent/s2"]
            );
        });
        app.read_with(cx, |app, _| assert!(app.side.drop_target.is_none()));
    }

    #[gpui_kit::test]
    fn the_open_list_grows_as_it_is_scrolled(cx: &mut TestAppContext) {
        let (handle, app, _rpc, _requests) = app(50, cx);
        assert_eq!(shown_ids(&app, cx).len(), super::OPEN_PAGE);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let row = vcx.debug_bounds("tab-test-agent/s1").unwrap().center();
        for _ in 0..3 {
            vcx.simulate_event(ScrollEvent {
                position: row,
                delta: ScrollDelta::Pixels(point(px(0.), px(-5000.))),
                ..Default::default()
            });
            render(handle, cx);
        }
        assert!(shown_ids(&app, cx).len() > super::OPEN_PAGE);
    }

    #[gpui_kit::test]
    fn the_line_above_the_first_row_is_not_clipped(cx: &mut TestAppContext) {
        let (handle, app, _rpc, _requests) = app(3, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let from = vcx.debug_bounds("tab-test-agent/s3").unwrap().center();
        let s1 = vcx.debug_bounds("tab-test-agent/s1").unwrap();
        let s2 = vcx.debug_bounds("tab-test-agent/s2").unwrap();
        vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        vcx.simulate_mouse_move(
            point(from.x, from.y - px(10.)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        let to = point(s1.center().x, s1.center().y - px(4.));
        vcx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        app.read_with(cx, |app, _| {
            let t = app.side.drop_target.as_ref().expect("drop line");
            assert!(t.key.ends_with("/s1") && !t.after);
        });
        // The list scrolls from s1's top: the line stays inside the row.
        render(handle, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let line = vcx.debug_bounds("tab-drop-line").expect("line shown");
        assert!(line.top() >= s1.top() && line.bottom() <= s1.bottom());

        // Between rows it sits in the gap, as before.
        let to = point(s2.center().x, s2.center().y - px(4.));
        vcx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        render(handle, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let line = vcx.debug_bounds("tab-drop-line").expect("line shown");
        assert!(line.top() < s2.top() && line.bottom() > s1.bottom());
        vcx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    }

    #[gpui_kit::test]
    fn waiting_sessions_and_troubled_agents_are_marked(cx: &mut TestAppContext) {
        let (handle, app, _rpc, _requests) = app(3, cx);
        let store = app.read_with(cx, |app, _| app.store.clone());
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.request_for_test(
                    "test-agent",
                    9,
                    "session/request_permission",
                    json!({ "sessionId": "s2",
                        "toolCall": { "toolCallId": "t1", "title": "Run ls", "kind": "execute" },
                        "options": [{ "optionId": "o", "name": "Allow", "kind": "allow_once" }] }),
                    cx,
                )
            })
        });
        render(handle, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("wait-s2").is_some());
        assert!(vcx.debug_bounds("wait-s1").is_none());
        assert!(vcx.debug_bounds("agents-attention").is_none());

        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.runtime_mut_for_test("other").state = ConnState::Failed("spawn failed".into());
                s.configs.push(crate::store::AgentConfig {
                    id: "other".into(),
                    name: "Other".into(),
                    ..s.configs[0].clone()
                });
            })
        });
        render(handle, cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("agents-attention").is_some());
        store.read_with(cx, |s, _| {
            assert_eq!(
                s.agents_needing_attention(),
                [("Other".to_string(), "failed: spawn failed".to_string())]
            );
        });
    }
}
