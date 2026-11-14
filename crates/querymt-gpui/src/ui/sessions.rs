//! Sessions page: search, lifecycle tabs, pinned group, sessions grouped by workspace.

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{QmtApp, SessionTab, kit};
use crate::store::SessionSummary;
use crate::theme::{Palette, palette};
use crate::util::{short_time, workspace_name};

const GROUP_ROWS: usize = 8;
const PINNED_KEY: &str = "\u{0}pinned";

fn in_tab(s: &SessionSummary, tab: SessionTab) -> bool {
    match tab {
        SessionTab::Open => s.open,
        SessionTab::Resolved => s.resolved && !s.archived,
        SessionTab::Archived => s.archived,
        SessionTab::All => true,
    }
}

impl QmtApp {
    pub(super) fn render_sessions(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let query = self.session_query.read(cx).value().trim().to_lowercase();
        let store = self.store.read(cx);
        let refreshing = store.configs.iter().any(|c| store.runtime(&c.id).is_some_and(|r| r.busy));
        let loading = store.connected_ids().iter().any(|id| store.runtime(id).is_some_and(|r| !r.sessions_loaded));
        let agent_name = |id: &str| store.config(id).map(|c| c.name.clone()).unwrap_or_default();

        let roots: Vec<SessionSummary> = store.all_sessions().into_iter().filter(|s| s.root).cloned().collect();
        let count = |tab| roots.iter().filter(|s| in_tab(s, tab)).count();
        let counts = [
            (SessionTab::Open, "Open", count(SessionTab::Open)),
            (SessionTab::Resolved, "Resolved", count(SessionTab::Resolved)),
            (SessionTab::Archived, "Archived", count(SessionTab::Archived)),
            (SessionTab::All, "All", roots.len()),
        ];
        let tab = self.session_tab;
        let mut visible: Vec<SessionSummary> = roots
            .into_iter()
            .filter(|s| in_tab(s, tab))
            .filter(|s| {
                query.is_empty()
                    || s.title.to_lowercase().contains(&query)
                    || s.cwd.to_lowercase().contains(&query)
                    || s.id.to_lowercase().contains(&query)
                    || agent_name(&s.agent_id).to_lowercase().contains(&query)
            })
            .collect();
        visible.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        // Pinned first, then workspaces ordered by their latest activity.
        let mut groups: Vec<(String, String, Vec<SessionSummary>)> = Vec::new();
        let pinned: Vec<SessionSummary> = visible.iter().filter(|s| s.pinned).cloned().collect();
        if !pinned.is_empty() {
            groups.push((PINNED_KEY.into(), "Pinned sessions".into(), pinned));
        }
        for s in visible.into_iter().filter(|s| !s.pinned) {
            match groups.iter_mut().find(|(key, _, _)| *key == s.cwd) {
                Some((_, _, list)) => list.push(s),
                None => groups.push((s.cwd.clone(), workspace_name(&s.cwd), vec![s])),
            }
        }

        // ---- header
        let header = h_flex()
            .gap_3()
            .child(div().flex_1().text_2xl().font_bold().child("Sessions"))
            .child(
                kit::loading_icon_button("sessions-refresh", IconName::RefreshCw, "Refresh sessions", refreshing, 24.)
                    .on_click(cx.listener(|this, _, _, cx| this.store.update(cx, |s, cx| s.refresh_all(cx)))),
            );
        let search = h_flex()
            .h(px(40.))
            .px_3()
            .gap_2()
            .rounded(px(10.))
            .bg(p.card)
            .child(kit::icon(IconName::Search, 15., p.dim))
            .child(Input::new(&self.session_query).appearance(false).flex_1());
        let tabs = h_flex().gap_5().border_b_1().border_color(p.border).children(counts.into_iter().map(
            |(t, label, n)| {
                let active = t == tab;
                h_flex()
                    .id(label)
                    .gap_1p5()
                    .pb_2()
                    .mb(px(-1.))
                    .cursor_pointer()
                    .text_sm()
                    .border_b_2()
                    .border_color(if active { p.text } else { transparent_black() })
                    .text_color(if active { p.text } else { p.muted })
                    .when(active, |el| el.font_medium())
                    .child(label)
                    .child(div().text_xs().text_color(p.dim).child(n.to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session_tab = t;
                        cx.notify();
                    }))
            },
        ));

        let body = if groups.is_empty() {
            let text = if loading {
                "Loading sessions…"
            } else if !query.is_empty() {
                "No sessions match your search."
            } else if store.connected_ids().is_empty() {
                "Connect an agent to see its sessions."
            } else {
                "No sessions here yet."
            };
            div().py_12().text_center().text_sm().text_color(p.dim).child(text).into_any_element()
        } else {
            let mut out = Vec::new();
            for (gix, (key, name, list)) in groups.into_iter().enumerate() {
                out.push(self.session_group(gix, key, name, list, &p, cx).into_any_element());
            }
            v_flex().gap_2().children(out).into_any_element()
        };

        div().w_full().flex().justify_center().px_8().py_6().child(
            v_flex().w_full().max_w(px(1040.)).gap_4().child(header).child(search).child(tabs).child(body),
        )
    }

    fn session_group(
        &self,
        gix: usize,
        key: String,
        name: String,
        list: Vec<SessionSummary>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pinned_group = key == PINNED_KEY;
        let collapsed = self.collapsed.contains(&key);
        let expanded = self.expanded.contains(&key);
        let total = list.len();
        let latest = list.first().map(|s| short_time(&s.updated_at)).unwrap_or_default();
        let running = list.iter().filter(|s| s.running()).count();
        let shown = if expanded { total } else { total.min(GROUP_ROWS) };
        let hover = p.card;
        let (k1, k2) = (key.clone(), key.clone());

        let head = h_flex()
            .id(("group-head", gix))
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(px(8.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(kit::icon(if pinned_group { IconName::Pin } else { IconName::FolderOpen }, 14., p.dim))
            .child(div().text_sm().font_semibold().child(name))
            .when(!pinned_group, |el| {
                el.child(div().min_w_0().truncate().text_xs().text_color(p.dim).font_family(kit::MONO).child(key.clone()))
            })
            .child(div().flex_1())
            .when(running > 0, |el| el.child(kit::tone_chip(format!("{running} running"), p.success)))
            .when(!pinned_group, |el| el.child(div().text_xs().text_color(p.dim).child(latest)))
            .child(div().text_xs().text_color(p.dim).child(total.to_string()))
            .child(kit::icon(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }, 14., p.dim))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&k1) {
                    this.collapsed.insert(k1.clone());
                }
                cx.notify();
            }));

        let mut rows = Vec::new();
        if !collapsed {
            for (ix, s) in list.into_iter().take(shown).enumerate() {
                rows.push(self.session_row(gix * 10_000 + ix, s, pinned_group, p, cx).into_any_element());
            }
            if total > shown || expanded && total > GROUP_ROWS {
                let more = total - shown;
                rows.push(
                    div()
                        .id(("group-more", gix))
                        .ml(px(34.))
                        .py_1()
                        .text_xs()
                        .font_medium()
                        .text_color(p.accent)
                        .cursor_pointer()
                        .child(if more > 0 { format!("Show {more} more") } else { "Show less".into() })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded.remove(&k2) {
                                this.expanded.insert(k2.clone());
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        v_flex().child(head).children(rows)
    }

    fn session_row(&self, ix: usize, s: SessionSummary, in_pinned: bool, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = p.card;
        let (agent, id, pinned) = (s.agent_id.clone(), s.id.clone(), s.pinned);
        let status = if s.archived {
            Some(("archived", p.status_muted))
        } else if s.resolved {
            Some(("resolved", p.success))
        } else {
            None
        };
        h_flex()
            .id(("session-row", ix))
            .group("srow")
            .cursor_pointer()
            .on_click({
                let s = s.clone();
                cx.listener(move |this, _, _, cx| this.open_summary(&s, cx))
            })
            .gap_3()
            .pl(px(10.))
            .pr_2()
            .py_2()
            .rounded(px(10.))
            .hover(move |el| el.bg(hover))
            .child(kit::identicon(&s.id, 18.))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().min_w_0().truncate().text_sm().font_semibold().child(s.display_title()))
                            .when(s.running(), |el| el.child(kit::spinner(("row-spin", ix), 13., p.success)))
                            .when_some(status, |el, (t, c)| el.child(kit::tone_chip(t, c))),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(p.dim)
                            .when(in_pinned, |el| el.child(workspace_name(&s.cwd)).child("·"))
                            .child(div().font_family(kit::MONO).child(s.short_id()))
                            .when(s.messages > 0, |el| el.child("·").child(format!("{} msgs", s.messages))),
                    ),
            )
            .child(
                div()
                    .when(!pinned, |el| el.opacity(0.).group_hover("srow", |st| st.opacity(1.)))
                    .child(
                        kit::icon_button(("pin", ix), if pinned { IconName::PinOff } else { IconName::Pin }, if pinned { "Unpin" } else { "Pin" })
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |st, cx| st.set_pinned(&agent, &id, !pinned, cx))
                            })),
                    ),
            )
            .child(div().flex_none().w(px(48.)).text_right().text_xs().text_color(p.dim).child(short_time(&s.updated_at)))
    }
}
