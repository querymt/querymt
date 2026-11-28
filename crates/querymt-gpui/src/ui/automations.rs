//! Scheduled prompts: the automation catalog and its creation dialog
//! (desktop `routes/automations` and `ScheduleDialog`).
//!
//! Both dialogs are the app's own centred modals, like Add agent and New
//! invite: they appear without the kit dialog's slide.

use std::collections::HashSet;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::agents::modal_scrim;
use super::kit::ButtonExt as _;
use super::smooth_scroll::SmoothScroll;
use super::workspace_select::WorkspaceField;
use super::{Page, QmtApp, kit};
use crate::store::automations::{
    CREATE, NewSchedule, Schedule, ScheduleAction, ScheduleActionState, ScheduleTarget,
};
use crate::store::{AgentRuntime, AgentsStore, SessionSummary};
use crate::theme::{Palette, palette};
use crate::util::{absolute_time, now_ms, parse_rfc3339_ms};

/// Most sessions the target picker lists; search narrows the rest.
const PICKER_MAX: usize = 100;
/// Row action buttons, the size of the Sessions row menu.
const ROW_BTN: f32 = 28.;

/// (agent, node, schedule id): a schedule's identity across peers.
type RowKey = (String, Option<String>, String);

#[derive(Default)]
pub(super) struct AutomationPage {
    agent: Option<String>,
    details: HashSet<RowKey>,
    form: Option<Entity<ScheduleForm>>,
    confirm_delete: Option<(String, Schedule)>,
    /// The header's agent picker.
    pills: PillMenus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Group {
    Attention,
    Active,
    Paused,
    Completed,
}

impl Group {
    const ALL: [Self; 4] = [Self::Attention, Self::Active, Self::Paused, Self::Completed];

    /// Desktop `classifySchedule`, plus the agent's `exhausted` state.
    fn of(schedule: &Schedule) -> Self {
        let state = schedule.state.to_lowercase();
        if schedule.consecutive_failures > 0 || state.contains("fail") || state.contains("error") {
            Self::Attention
        } else if matches!(state.as_str(), "paused" | "suspended") {
            Self::Paused
        } else if matches!(
            state.as_str(),
            "exhausted"
                | "completed"
                | "complete"
                | "finished"
                | "cancelled"
                | "canceled"
                | "deleted"
                | "disabled"
        ) || schedule
            .max_runs
            .is_some_and(|max| schedule.run_count >= max)
        {
            Self::Completed
        } else {
            Self::Active
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Attention => "Needs attention",
            Self::Active => "Active",
            Self::Paused => "Paused",
            Self::Completed => "Completed",
        }
    }

    fn sort(self, items: &mut [&Schedule]) {
        items.sort_by(|a, b| match self {
            Self::Attention => b
                .consecutive_failures
                .cmp(&a.consecutive_failures)
                .then_with(|| b.updated_at.cmp(&a.updated_at)),
            // Soonest first; schedules without a next run last.
            Self::Active => a
                .next_run_at
                .is_none()
                .cmp(&b.next_run_at.is_none())
                .then_with(|| a.next_run_at.cmp(&b.next_run_at)),
            Self::Paused | Self::Completed => b.updated_at.cmp(&a.updated_at),
        });
    }
}

fn schedule_agents(store: &AgentsStore, method: Option<&str>) -> Vec<(String, String)> {
    store
        .configs
        .iter()
        .filter(|c| {
            store.runtime(&c.id).is_some_and(|r| {
                r.supports_schedules() && method.is_none_or(|m| r.supports_method(m))
            })
        })
        .map(|c| (c.id.clone(), c.name.clone()))
        .collect()
}

impl QmtApp {
    fn automation_agent(&self, cx: &App) -> Option<String> {
        let store = self.store.read(cx);
        self.automations
            .agent
            .as_ref()
            .filter(|id| {
                store
                    .runtime(id)
                    .is_some_and(AgentRuntime::supports_schedules)
            })
            .cloned()
            .or_else(|| {
                schedule_agents(store, None)
                    .into_iter()
                    .next()
                    .map(|(id, _)| id)
            })
    }

    pub(super) fn load_automations(&self, refresh: bool, cx: &mut Context<Self>) {
        let Some(id) = self.automation_agent(cx) else {
            return;
        };
        let should_load = refresh
            || self.store.read(cx).runtime(&id).is_some_and(|r| {
                !r.schedules.loaded && !r.schedules.loading && r.schedules.error.is_none()
            });
        if should_load {
            self.store
                .update(cx, |store, cx| store.refresh_schedules(&id, cx));
        }
    }

    pub(super) fn has_automation_target(&self, current: bool, cx: &App) -> bool {
        let store = self.store.read(cx);
        if current {
            self.page == Page::Session
                && self.main_view(cx).is_some_and(|s| {
                    !s.loading
                        && store
                            .runtime(&s.agent_id)
                            .is_some_and(|r| r.supports_schedules() && r.supports_method(CREATE))
                })
        } else {
            !schedule_agents(store, Some(CREATE)).is_empty()
        }
    }

    /// Open New automation; `current` targets the open session.
    pub(super) fn open_automation_dialog(
        &mut self,
        current: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let choices = schedule_agents(store, Some(CREATE));
        let selected = if current {
            self.main_view(cx)
                .filter(|s| self.page == Page::Session && !s.loading)
                .filter(|s| choices.iter().any(|(id, _)| id == &s.agent_id))
                .map(|s| {
                    let summary = store.session_summary(&s.agent_id, &s.session_id);
                    let choice = summary.map_or_else(
                        || SessionChoice {
                            id: s.session_id.clone(),
                            node: None,
                            title: s.title.clone(),
                            meta: short_id(&s.session_id),
                        },
                        SessionChoice::from,
                    );
                    (s.agent_id.clone(), Some(choice))
                })
        } else {
            self.automation_agent(cx)
                .filter(|agent| choices.iter().any(|(id, _)| id == agent))
                .or_else(|| choices.first().map(|(id, _)| id.clone()))
                .map(|id| (id, None))
        };
        let Some((agent, session)) = selected else {
            return;
        };
        let prompt = if self.page == Page::Start {
            self.prompt.read(cx).value().to_string()
        } else {
            String::new()
        };
        let prefill = SchedulePrefill {
            agent,
            session,
            prompt,
            cwd: self.cwd.read(cx).value().to_string(),
        };
        let (store, owner) = (self.store.clone(), cx.entity().downgrade());
        let scroll = self.smooth_scroll.clone();
        let form = cx.new(|cx| ScheduleForm::new(store, owner, prefill, scroll, window, cx));
        form.read(cx)
            .prompt
            .clone()
            .update(cx, |s, cx| s.focus(window, cx));
        self.automations.form = Some(form);
        cx.notify();
    }

    /// Close the topmost automation surface: an open menu, then the dialog
    /// (a form that is submitting stays).
    pub(super) fn close_automation_overlay(&mut self, cx: &mut App) -> bool {
        if self.automations.confirm_delete.take().is_some() || self.automations.pills.close() {
            return true;
        }
        if let Some(form) = self.automations.form.clone() {
            if !form.update(cx, |form, cx| form.escape(cx)) {
                self.automations.form = None;
            }
            return true;
        }
        false
    }

    pub(super) fn render_automation_overlay(
        &self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = *palette(cx);
        if let Some((agent, schedule)) = self.automations.confirm_delete.clone() {
            let target = self
                .store
                .read(cx)
                .runtime(&agent)
                .map_or_else(|| "Its session".into(), |rt| target_label(rt, &schedule));
            let card = self
                .modal_card(&p)
                .w(px(440.))
                .child(div().text_lg().font_semibold().child(format!(
                    "Delete \u{201c}{}\u{201d}?",
                    trigger_label(&schedule.trigger)
                )))
                .child(div().text_sm().text_color(p.muted).child(format!(
                    "The automation stops running. {target} and its history are kept."
                )))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            kit::flat_btn("automation-delete-cancel", true, true, &p)
                                .child("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.automations.confirm_delete = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            kit::danger_btn("automation-delete-ok", true, &p)
                                .child("Delete")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.automations.confirm_delete = None;
                                    this.store.update(cx, |s, cx| {
                                        s.run_schedule_action(
                                            &agent,
                                            &schedule,
                                            ScheduleAction::Delete,
                                            cx,
                                        )
                                    });
                                    cx.notify();
                                })),
                        ),
                );
            return Some(self.centered(
                modal_scrim("scrim-automation-delete", radii, &p),
                card,
                cx,
            ));
        }
        let form = self.automations.form.clone()?;
        let card = self
            .modal_card(&p)
            .w(px(560.))
            .max_w(relative(0.92))
            .child(form);
        Some(self.centered(modal_scrim("scrim-automation", radii, &p), card, cx))
    }

    pub(super) fn render_automations(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let agent = self.automation_agent(cx);
        let choices = schedule_agents(self.store.read(cx), None);

        // Header: agent, New automation, then refresh (rightmost on every page).
        let mut actions = Vec::new();
        if let Some(id) = &agent
            && choices.len() > 1
        {
            actions.push(
                pill_select(
                    PillSelect {
                        id: "automation-agent",
                        icon: IconName::Bot,
                        options: choices,
                        selected: id.clone(),
                        open: self.automations.pills.is_open("automation-agent"),
                        enabled: true,
                        quiet: false,
                        align_right: true,
                        menu_w: 220.,
                        scroll: None,
                    },
                    |this: &mut Self, id, cx| {
                        this.automations.agent = Some(id);
                        this.load_automations(true, cx);
                    },
                    &p,
                    cx,
                )
                .into_any_element(),
            );
        }
        let store = self.store.read(cx);
        let rt = agent.as_deref().and_then(|id| store.runtime(id));
        let loading = rt.is_some_and(|r| r.schedules.loading);
        let can_create = rt.is_some_and(|r| r.supports_method(CREATE));
        actions.push(
            kit::flat_btn("new-automation", can_create, true, &p)
                .child(kit::icon(IconName::Plus, 14., p.text))
                .child("New automation")
                .when(can_create, |el| {
                    el.on_click(cx.listener(|this, _, window, cx| {
                        this.open_automation_dialog(false, window, cx)
                    }))
                })
                .into_any_element(),
        );
        actions.push(
            kit::loading_icon_button(
                "refresh-automations",
                IconName::RefreshCw,
                "Refresh automations",
                loading,
                24.,
            )
            .inactive(agent.is_none())
            .on_click(cx.listener(|this, _, _, cx| this.load_automations(true, cx)))
            .into_any_element(),
        );
        let header = kit::page_header(
            "Automations",
            None,
            Some("Scheduled tasks, run status, and controls."),
            actions,
            &p,
        );
        let mut content = vec![header.into_any_element()];

        let (Some(agent), Some(rt)) = (agent, rt) else {
            content.push(
                state_card(
                    IconName::CalendarClock,
                    "Automations are not available",
                    "Connect an agent that supports scheduled tasks to create and manage automations.",
                    Some(
                        kit::accent_btn("automation-agents", true, true, &p)
                            .child("Open agents")
                            .on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx)))
                            .into_any_element(),
                    ),
                    &p,
                )
                .into_any_element(),
            );
            return kit::page(content);
        };
        let catalog = &rt.schedules;
        let retry = |id: &'static str, cx: &Context<Self>| {
            kit::flat_btn(id, true, true, &p)
                .child(kit::icon(IconName::RotateCcw, 14., p.text))
                .child("Try again")
                .on_click(cx.listener(|this, _, _, cx| this.load_automations(true, cx)))
        };

        if !catalog.loaded {
            content.push(
                match &catalog.error {
                    Some(error) => state_card(
                        IconName::CircleAlert,
                        "Automations could not be loaded",
                        error.clone(),
                        Some(retry("automations-retry", cx).into_any_element()),
                        &p,
                    ),
                    None => kit::card(&p).child(
                        h_flex()
                            .justify_center()
                            .gap_2()
                            .py_8()
                            .text_sm()
                            .text_color(p.muted)
                            .child(kit::spinner("automation-list-loading", 14., p.muted))
                            .child("Loading automations…"),
                    ),
                }
                .into_any_element(),
            );
            return kit::page(content);
        }

        let mut groups: [Vec<&Schedule>; 4] = Default::default();
        for schedule in &catalog.items {
            groups[Group::of(schedule) as usize].push(schedule);
        }
        content.push(summary(&groups, &p).into_any_element());

        // A refresh failed; the last list is still shown.
        if let Some(error) = &catalog.error {
            content.push(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(px(10.))
                    .bg(p.danger.opacity(0.1))
                    .text_sm()
                    .child(kit::icon(IconName::TriangleAlert, 15., p.danger))
                    .child(div().flex_1().min_w_0().child(error.clone()))
                    .child(retry("automations-retry-inline", cx))
                    .into_any_element(),
            );
        }

        if catalog.items.is_empty() {
            content.push(
                state_card(
                    IconName::CalendarClock,
                    "No automations yet",
                    "Schedule prompts to run against new or existing sessions.",
                    can_create.then(|| {
                        kit::accent_btn("empty-new-automation", true, true, &p)
                            .child(kit::icon(IconName::Plus, 14., p.on_accent))
                            .child("New automation")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_automation_dialog(false, window, cx)
                            }))
                            .into_any_element()
                    }),
                    &p,
                )
                .into_any_element(),
            );
            return kit::page(content);
        }

        for (group, mut items) in Group::ALL.into_iter().zip(groups) {
            if items.is_empty() {
                continue;
            }
            group.sort(&mut items);
            let count = items.len();
            let rows: Vec<AnyElement> = items
                .into_iter()
                .enumerate()
                .map(|(ix, s)| self.automation_row(&agent, rt, s, group, ix == 0, &p, cx))
                .collect();
            content.push(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().font_semibold().child(group.label()))
                            .child(kit::chip(count.to_string(), &p)),
                    )
                    .child(kit::card(&p).children(rows).child(kit::card_mask(&p)))
                    .into_any_element(),
            );
        }
        kit::page(content)
    }

    #[allow(clippy::too_many_arguments)]
    fn automation_row(
        &self,
        agent: &str,
        rt: &AgentRuntime,
        schedule: &Schedule,
        group: Group,
        first: bool,
        p: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (node, schedule_id) = schedule.key();
        let key: RowKey = (agent.to_owned(), node.clone(), schedule_id);
        let row_key = format!(
            "{agent}:{}:{}",
            node.as_deref().unwrap_or_default(),
            schedule.public_id
        );
        let expanded = self.automations.details.contains(&key);
        let (pending, error) = match rt.schedules.actions.get(&schedule.key()) {
            Some(ScheduleActionState::Pending(action)) => (Some(*action), None),
            Some(ScheduleActionState::Failed(error)) => (None, Some(error.clone())),
            None => (None, None),
        };
        let label = trigger_label(&schedule.trigger);
        let running = schedule.state.eq_ignore_ascii_case("running");

        let action = |what: ScheduleAction, icon: IconName, tip: &'static str| {
            let (agent, schedule) = (agent.to_owned(), schedule.clone());
            kit::row_icon_button(kit::keyed(tip, row_key.clone()), icon, tip, p, cx)
                .size(px(ROW_BTN))
                .accessibility_label(format!("{tip}: {label}"))
                .loading(pending == Some(what))
                .inactive(pending.is_some() || !rt.supports_method(what.method()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |s, cx| {
                        s.run_schedule_action(&agent, &schedule, what, cx)
                    })
                }))
        };

        let menu = {
            let can_run = rt.supports_method(ScheduleAction::Run.method());
            let can_delete = rt.supports_method(ScheduleAction::Delete.method());
            let (store, app) = (self.store.clone(), cx.entity().downgrade());
            let (agent, schedule, key) = (agent.to_owned(), schedule.clone(), key.clone());
            let busy = pending.is_some();
            kit::row_icon_button(
                kit::keyed("automation-menu", row_key.clone()),
                IconName::Ellipsis,
                "Automation actions",
                p,
                cx,
            )
            .size(px(ROW_BTN))
            .loading(pending == Some(ScheduleAction::Delete))
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let (a1, a2, a3) = (app.clone(), app.clone(), store.clone());
                let (key, id) = (key.clone(), schedule.public_id.clone());
                let (run_agent, run_schedule) = (agent.clone(), schedule.clone());
                let delete = (agent.clone(), schedule.clone());
                menu.when(group == Group::Paused, |m| {
                    m.item(
                        PopupMenuItem::new("Run now")
                            .icon(Icon::new(IconName::Zap))
                            .disabled(!can_run || busy)
                            .on_click(move |_, _, cx| {
                                a3.update(cx, |s, cx| {
                                    s.run_schedule_action(
                                        &run_agent,
                                        &run_schedule,
                                        ScheduleAction::Run,
                                        cx,
                                    )
                                })
                            }),
                    )
                })
                .item(
                    PopupMenuItem::new(if expanded {
                        "Hide details"
                    } else {
                        "Show details"
                    })
                    .icon(Icon::new(IconName::Info))
                    .on_click(move |_, _, cx| {
                        let _ = a1.update(cx, |this, cx| {
                            this.toggle_automation_details(&key);
                            cx.notify();
                        });
                    }),
                )
                .item(
                    PopupMenuItem::new("Copy automation ID")
                        .icon(Icon::new(IconName::Copy))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new("Delete automation…")
                        .icon(Icon::new(IconName::Trash))
                        .disabled(!can_delete || busy)
                        .on_click(move |_, _, cx| {
                            let _ = a2.update(cx, |this, cx| {
                                this.automations.confirm_delete = Some(delete.clone());
                                cx.notify();
                            });
                        }),
                )
            })
        };

        let chip = match group {
            Group::Attention => Some(kit::tone_chip("Needs attention", p.danger)),
            _ if running => Some(kit::tone_chip("Running", p.success)),
            Group::Paused => Some(kit::chip("Paused", p)),
            Group::Completed => Some(kit::chip("Completed", p)),
            Group::Active => None,
        };
        let text = v_flex()
            .flex_1()
            .min_w_0()
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(kit::rpx(13.4))
                            .font_semibold()
                            .truncate()
                            .child(label.clone()),
                    )
                    .children(chip),
            )
            .child(
                div()
                    .mt(px(3.2))
                    .text_xs()
                    .text_color(if group == Group::Attention {
                        p.warn
                    } else {
                        p.muted
                    })
                    .child(timing(schedule, group, running)),
            )
            .child(
                div()
                    .mt(px(1.))
                    .text_xs()
                    .text_color(p.dim)
                    .truncate()
                    .child(target_label(rt, schedule)),
            )
            .when_some(error, |el, error| {
                el.child(
                    h_flex()
                        .mt(px(3.2))
                        .gap_1p5()
                        .text_xs()
                        .text_color(p.danger)
                        .child(kit::icon(IconName::CircleAlert, 12., p.danger))
                        .child(div().min_w_0().child(error)),
                )
            });
        let buttons = h_flex()
            .id(kit::keyed("automation-actions", row_key.clone()))
            .flex_none()
            .gap_1()
            .on_click(|_, _, cx| cx.stop_propagation())
            .map(|el| match group {
                Group::Paused => el.child(action(ScheduleAction::Resume, IconName::Play, "Resume")),
                Group::Completed => el,
                _ => el
                    .child(action(ScheduleAction::Run, IconName::Zap, "Run now"))
                    .child(action(ScheduleAction::Pause, IconName::Pause, "Pause")),
            })
            .child(div().ml_1().child(menu));

        let (divider, hover) = kit::subtle(p);
        v_flex()
            .id(kit::keyed("automation-row", row_key.clone()))
            .when(!first, |el| el.border_t_1().border_color(divider))
            .cursor_pointer()
            .hover(move |el| el.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_automation_details(&key);
                cx.notify();
            }))
            .child(
                kit::card_row(true, p)
                    .child(kit::icon_tile(
                        IconName::CalendarClock,
                        group == Group::Active,
                        p,
                    ))
                    .child(text)
                    .child(buttons),
            )
            .when(expanded, |el| el.child(details(schedule, &row_key, p, cx)))
            .into_any_element()
    }

    fn toggle_automation_details(&mut self, key: &RowKey) {
        if !self.automations.details.remove(key) {
            self.automations.details.insert(key.clone());
        }
    }
}

/// Counts of the catalog, as the Mesh summary tiles.
fn summary(groups: &[Vec<&Schedule>; 4], p: &Palette) -> Div {
    let count = |g: Group| groups[g as usize].len();
    let attention = count(Group::Attention);
    let cell = |label: &'static str, value: usize, color: Hsla| {
        v_flex()
            .flex_1()
            .px_4()
            .py_3()
            .gap_0p5()
            .rounded(px(12.))
            .bg(p.card)
            .child(div().text_xs().text_color(p.muted).child(label))
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .text_color(color)
                    .child(value.to_string()),
            )
    };
    h_flex()
        .gap_2()
        .child(cell("Total", groups.iter().map(Vec::len).sum(), p.text))
        .child(cell("Active", count(Group::Active), p.text))
        .child(cell("Paused", count(Group::Paused), p.text))
        .child(cell(
            "Needs attention",
            attention,
            if attention > 0 { p.danger } else { p.text },
        ))
}

/// Centred icon, title, explanation and an optional action, in a card.
fn state_card(
    icon: IconName,
    title: &'static str,
    body: impl Into<SharedString>,
    action: Option<AnyElement>,
    p: &Palette,
) -> Div {
    kit::card(p)
        .items_center()
        .gap_2()
        .p_8()
        .child(kit::icon_tile(icon, false, p))
        .child(div().font_semibold().child(title))
        .child(
            div()
                .max_w(px(440.))
                .text_center()
                .text_sm()
                .text_color(p.muted)
                .child(body.into()),
        )
        .children(action.map(|a| div().pt_2().child(a)))
}

/// Facts of an expanded row, under its title.
fn details(s: &Schedule, row_key: &str, p: &Palette, cx: &App) -> Div {
    let mono_family = kit::mono_family(cx);
    let runs = s.max_runs.map_or_else(
        || s.run_count.to_string(),
        |max| format!("{} of {max}", s.run_count),
    );
    let time = |t: Option<&str>, none: &str| {
        t.map_or_else(
            || none.to_owned(),
            |t| absolute_time(t).unwrap_or_else(|| t.to_owned()),
        )
    };
    let item = |label: &'static str, value: String, mono: bool| {
        h_flex()
            .w(relative(0.5))
            .min_w_0()
            .gap_3()
            .py_0p5()
            .child(div().w(px(96.)).flex_none().text_color(p.dim).child(label))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .when(mono, |el| el.font_family(mono_family.clone()))
                    .child(value),
            )
    };
    h_flex()
        .flex_wrap()
        // Under the title: row padding, icon tile and gap.
        .pl(px(64.8))
        .pr(px(14.4))
        .pb(px(12.))
        .text_xs()
        .child(
            item("Automation", short_id(&s.public_id), true).child(kit::copy_button(
                kit::keyed("copy-automation-id", row_key.to_owned()),
                s.public_id.clone(),
                "Copy automation ID",
            )),
        )
        .child(item("Task", short_id(&s.task_public_id), true))
        .child(item(
            "Trigger",
            trigger_expression(&s.trigger).unwrap_or_else(|| trigger_label(&s.trigger)),
            false,
        ))
        .child(item("Runs", runs, false))
        .child(item(
            "Last run",
            time(s.last_run_at.as_deref(), "Not run yet"),
            false,
        ))
        .child(item(
            "Next run",
            time(s.next_run_at.as_deref(), "None"),
            false,
        ))
        .child(item(
            "Runtime limit",
            duration_label(s.max_runtime_seconds),
            false,
        ))
        .child(item("Updated", time(Some(&s.updated_at), ""), false))
}

/// The session a schedule runs in: peer, title and workspace when known.
fn target_label(rt: &AgentRuntime, s: &Schedule) -> String {
    let node = s.node_id.as_deref();
    let session = rt.sessions.iter().find(|x| {
        x.id == s.session_public_id && x.remote.as_ref().map(|r| r.node_id.as_str()) == node
    });
    let peer = node.map(|n| {
        session
            .and_then(|x| x.remote.as_ref())
            .map(|r| r.name())
            .or_else(|| {
                rt.mesh_nodes
                    .iter()
                    .find(|m| m.id == n && !m.label.is_empty())
                    .map(|m| m.label.clone())
            })
            .unwrap_or_else(|| super::mesh::short_peer(n))
    });
    let mut parts: Vec<String> = peer.into_iter().collect();
    match session {
        Some(x) => {
            parts.push(x.display_title());
            if !x.cwd.is_empty() {
                parts.push(compact_path(&x.cwd));
            }
        }
        None => parts.push(format!("Session {}", short_id(&s.session_public_id))),
    }
    parts.join(" · ")
}

/// Desktop `scheduleTimingSummary`.
fn timing(s: &Schedule, group: Group, running: bool) -> String {
    let n = s.consecutive_failures;
    if n > 0 {
        return format!(
            "{n} consecutive {}",
            if n == 1 { "failure" } else { "failures" }
        );
    }
    match group {
        Group::Attention => "Stopped after failing".into(),
        Group::Paused => s
            .last_run_at
            .as_deref()
            .and_then(relative_time)
            .map_or_else(|| "Paused".into(), |t| format!("Paused · last ran {t}")),
        Group::Completed => format!(
            "Completed after {} {}",
            s.run_count,
            if s.run_count == 1 { "run" } else { "runs" }
        ),
        Group::Active if running => "Running now".into(),
        Group::Active => s
            .next_run_at
            .as_deref()
            .and_then(relative_time)
            .map_or_else(
                || "Next run unavailable".into(),
                |t| format!("Next run {t}"),
            ),
    }
}

/// `in 5m` / `5m ago`, rounded to minutes, hours or days.
fn relative_time(timestamp: &str) -> Option<String> {
    relative_to(parse_rfc3339_ms(timestamp)?, now_ms())
}

fn relative_to(at_ms: f64, now: f64) -> Option<String> {
    let delta = at_ms - now;
    let minutes = ((delta.abs() / 60_000.).round() as u64).max(1);
    let (count, unit) = if minutes < 60 {
        (minutes, "m")
    } else if minutes < 60 * 24 {
        ((minutes as f64 / 60.).round() as u64, "h")
    } else {
        ((minutes as f64 / 1440.).round() as u64, "d")
    };
    Some(if delta > 0. {
        format!("in {count}{unit}")
    } else {
        format!("{count}{unit} ago")
    })
}

/// Last two segments of a path: enough to recognise a workspace.
fn compact_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        path.to_owned()
    } else {
        parts[parts.len() - 2..].join("/")
    }
}

fn short_id(id: &str) -> String {
    if id.chars().count() > 10 {
        format!("{}…", id.chars().take(8).collect::<String>())
    } else {
        id.to_owned()
    }
}

/// Desktop `formatDuration`.
fn duration_label(seconds: u64) -> String {
    match seconds {
        0 => "None".into(),
        1..60 => format!("{seconds}s"),
        _ => {
            let minutes = (seconds as f64 / 60.).round() as u64;
            match (minutes / 60, minutes % 60) {
                (0, m) => format!("{m}m"),
                (h, 0) => format!("{h}h"),
                (h, m) => format!("{h}h {m}m"),
            }
        }
    }
}

fn interval_label(seconds: u64) -> String {
    let (count, unit) = [(86_400, "day"), (3600, "hour"), (60, "minute")]
        .into_iter()
        .find(|(size, _)| seconds.is_multiple_of(*size))
        .map_or((seconds, "second"), |(size, unit)| (seconds / size, unit));
    if count == 1 {
        format!("Every {unit}")
    } else {
        format!("Every {count} {unit}s")
    }
}

fn trigger_expression(trigger: &Value) -> Option<String> {
    trigger
        .get("expr")
        .or_else(|| trigger.get("cron"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Plain language for the cron shapes the desktop dialog produced.
fn describe_cron(expression: &str) -> String {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    let clock = |h: &str, m: &str| {
        let (h, m) = (h.parse::<u8>().ok()?, m.parse::<u8>().ok()?);
        (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
    };
    match fields.as_slice() {
        ["0", "*", "*", "*", "*"] => "Every hour".into(),
        [m, h, "*", "*", "*"] if let Some(t) = clock(h, m) => format!("Every day at {t}"),
        [m, h, "*", "*", "1-5"] if let Some(t) = clock(h, m) => format!("Weekdays at {t}"),
        _ => format!("Cron: {expression}"),
    }
}

/// Desktop `scheduleTriggerLabel`.
fn trigger_label(trigger: &Value) -> String {
    match trigger.get("type").and_then(Value::as_str) {
        Some("interval") => trigger
            .get("seconds")
            .and_then(Value::as_u64)
            .filter(|s| *s > 0)
            .map_or_else(|| "Interval".into(), interval_label),
        Some("once_at") => trigger
            .get("at")
            .and_then(Value::as_str)
            .and_then(absolute_time)
            .map_or_else(|| "Run once".into(), |at| format!("Once on {at}")),
        Some("event_driven") => "Event-triggered".into(),
        _ => trigger_expression(trigger)
            .map_or_else(|| "Scheduled automation".into(), |e| describe_cron(&e)),
    }
}

// ---- Pill selects -------------------------------------------------------

/// The open pill menu of a view, and the outside mouse down that closed one
/// (so the click on its trigger that follows does not reopen it).
#[derive(Default)]
pub(super) struct PillMenus {
    open: Option<&'static str>,
    dismissed: kit::Dismissed,
}

impl PillMenus {
    fn is_open(&self, id: &str) -> bool {
        self.open == Some(id)
    }

    fn close(&mut self) -> bool {
        self.open.take().is_some()
    }
}

trait PillHost: 'static + Sized {
    fn pills(&mut self) -> &mut PillMenus;
}

impl PillHost for QmtApp {
    fn pills(&mut self) -> &mut PillMenus {
        &mut self.automations.pills
    }
}

impl PillHost for ScheduleForm {
    fn pills(&mut self) -> &mut PillMenus {
        &mut self.pills
    }
}

/// Open or close `id` from its trigger.
fn toggle_pill<V: PillHost>(id: &'static str, ev: &ClickEvent, host: &mut V) -> bool {
    let pills = host.pills();
    if pills.dismissed.closed_by(id, ev) {
        return false;
    }
    pills.open = if pills.open == Some(id) {
        None
    } else {
        Some(id)
    };
    pills.open.is_some()
}

struct PillSelect {
    id: &'static str,
    icon: IconName,
    options: Vec<(String, String)>,
    selected: String,
    open: bool,
    enabled: bool,
    /// Small muted trigger (time zone) rather than a full control.
    quiet: bool,
    align_right: bool,
    menu_w: f32,
    scroll: Option<ScrollHandle>,
}

/// The app's pill select (as Settings' auth method): a flat trigger and a
/// plain menu under it, without the kit dropdown's slide.
fn pill_select<V: PillHost>(
    s: PillSelect,
    pick: impl Fn(&mut V, String, &mut Context<V>) + Clone + 'static,
    p: &Palette,
    cx: &mut Context<V>,
) -> Div {
    let label = s
        .options
        .iter()
        .find(|(value, _)| *value == s.selected)
        .map_or_else(|| s.selected.clone(), |(_, label)| label.clone());
    let trigger_h = if s.quiet { 26. } else { 32. };
    let id = s.id;
    let selected_ix = s.options.iter().position(|(v, _)| *v == s.selected);
    let scroll = s.scroll.clone();
    let tone = if s.quiet { p.muted } else { p.text };
    let trigger = kit::flat_btn(id, s.enabled, true, p)
        .h(px(trigger_h))
        .when(s.quiet, |el| {
            el.px(px(8.)).text_size(kit::rpx(11.8)).text_color(p.muted)
        })
        .child(kit::icon(
            s.icon,
            13.,
            if s.quiet { p.dim } else { p.muted },
        ))
        .child(
            div()
                .max_w(px(200.))
                .truncate()
                .text_color(tone)
                .child(label),
        )
        .child(kit::icon(
            if s.open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            },
            12.,
            p.dim,
        ))
        .when(s.enabled, |el| {
            el.on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                if toggle_pill(id, ev, this)
                    && let (Some(scroll), Some(ix)) = (&scroll, selected_ix)
                {
                    scroll.scroll_to_item(ix);
                }
                cx.notify();
            }))
        });
    let menu = s.open.then(|| {
        let hover = p.card_hover;
        let rows: Vec<AnyElement> = s
            .options
            .into_iter()
            .map(|(value, text)| {
                let selected = value == s.selected;
                let pick = pick.clone();
                h_flex()
                    .id(kit::keyed(id, value.clone()))
                    .flex_none()
                    .h(px(30.))
                    .px_2()
                    .gap_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(kit::rpx(12.))
                    .hover(move |st| st.bg(hover))
                    .child(div().flex_1().min_w_0().truncate().child(text))
                    .when(selected, |el| {
                        el.child(kit::icon(IconName::Check, 13., p.accent))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pills().close();
                        if !selected {
                            pick(this, value.clone(), cx);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        deferred(
            v_flex()
                .id((id, 1usize))
                .absolute()
                .top(px(trigger_h + 4.))
                .map(|el| {
                    if s.align_right {
                        el.right_0()
                    } else {
                        el.left_0()
                    }
                })
                .w(px(s.menu_w))
                .max_h(px(280.))
                .overflow_y_scroll()
                .when_some(s.scroll.as_ref(), |el, scroll| el.track_scroll(scroll))
                .p_1()
                .rounded(px(10.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    let pills = this.pills();
                    if pills.is_open(id) {
                        pills.open = None;
                        pills.dismissed.record(id, ev);
                        cx.notify();
                    }
                }))
                .children(rows),
        )
        .with_priority(4)
    });
    div().relative().flex_none().child(trigger).children(menu)
}

/// Info icon with an explanation on hover (desktop `HelpPopover`).
fn help_icon(id: &'static str, text: &'static str, p: &Palette) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .child(kit::icon(IconName::Info, 13., p.dim))
        .tooltip(move |window, cx| kit::wrap_tip(text).build(window, cx))
}

// ---- New automation -----------------------------------------------------

#[derive(Clone)]
struct SessionChoice {
    id: String,
    node: Option<String>,
    title: String,
    /// Peer, workspace and short id.
    meta: String,
}

impl From<&SessionSummary> for SessionChoice {
    fn from(s: &SessionSummary) -> Self {
        let mut meta: Vec<String> = s.remote.iter().map(|r| r.name()).collect();
        if !s.cwd.is_empty() {
            meta.push(compact_path(&s.cwd));
        }
        meta.push(short_id(&s.id));
        Self {
            id: s.id.clone(),
            node: s.remote.as_ref().map(|r| r.node_id.clone()),
            title: s.display_title(),
            meta: meta.join(" · "),
        }
    }
}

struct SchedulePrefill {
    agent: String,
    session: Option<SessionChoice>,
    prompt: String,
    cwd: String,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Frequency {
    Minutes,
    Hourly,
    Daily,
    Weekdays,
    Custom,
}

impl Frequency {
    const ALL: [(Self, &'static str); 5] = [
        (Self::Minutes, "Minutes"),
        (Self::Hourly, "Hourly"),
        (Self::Daily, "Daily"),
        (Self::Weekdays, "Weekdays"),
        (Self::Custom, "Custom"),
    ];

    /// Daily, Weekdays and Custom need cron triggers, which the agent lacks.
    fn available(self) -> bool {
        matches!(self, Self::Minutes | Self::Hourly)
    }
}

const TIMEZONE_MENU: &str = "schedule-timezone";
const AGENT_MENU: &str = "schedule-agent";
const LIMITS_MENU: &str = "schedule-limits";

struct ScheduleForm {
    store: Entity<AgentsStore>,
    owner: WeakEntity<QmtApp>,
    agent: String,
    existing: bool,
    session: Option<SessionChoice>,
    workspace_input: Entity<InputState>,
    workspace: Entity<WorkspaceField>,
    search: Entity<InputState>,
    prompt: Entity<TextareaState>,
    minutes: Entity<InputState>,
    runs: Entity<InputState>,
    steps: Entity<InputState>,
    cost: Entity<InputState>,
    frequency: Frequency,
    /// Applies to cron schedules (Daily, Weekdays, Custom); kept across
    /// dialogs as the desktop did.
    timezone: String,
    time_zones: Vec<String>,
    timezone_scroll: ScrollHandle,
    pills: PillMenus,
    submitting: bool,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

/// Re-render the form as an input changes: what it may submit depends on it.
fn on_change<E: EventEmitter<InputEvent>>(
    input: &Entity<E>,
    cx: &mut Context<ScheduleForm>,
) -> Subscription {
    cx.subscribe(input, |_, _, ev: &InputEvent, cx| {
        if matches!(ev, InputEvent::Change) {
            cx.notify();
        }
    })
}

impl ScheduleForm {
    fn new(
        store: Entity<AgentsStore>,
        owner: WeakEntity<QmtApp>,
        prefill: SchedulePrefill,
        smooth_scroll: SmoothScroll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let SchedulePrefill {
            agent,
            session,
            prompt,
            cwd,
        } = prefill;
        let input = |placeholder: &'static str,
                     value: &str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            let value = value.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let workspace_input = input("/path/on/the/agent/machine", &cwd, window, cx);
        let workspace = {
            let (store, input, agent) = (store.clone(), workspace_input.clone(), agent.clone());
            cx.new(|cx| {
                let mut field = WorkspaceField::new(store, input, true, smooth_scroll, cx);
                field.set_agent(Some(agent), cx);
                field
            })
        };
        let search = input("Search by title, session ID or workspace", "", window, cx);
        let minutes = cx.new(|cx| {
            let mut state = InputState::new(window, cx).default_value("30");
            state.set_text_align(TextAlign::Center, cx);
            state
        });
        let runs = input("Unlimited", "", window, cx);
        let steps = input("Unlimited", "", window, cx);
        let cost = input("Unlimited", "", window, cx);
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("What should the agent do each time?")
                .default_value(prompt)
        });
        let mut subs: Vec<Subscription> = [&workspace_input, &minutes, &runs, &steps, &cost]
            .into_iter()
            .map(|input| on_change(input, cx))
            .collect();
        subs.push(on_change(&prompt, cx));
        // Enter in the search picks the first match.
        subs.push(cx.subscribe(&search, |this, _, ev: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = ev
                && let Some(first) = this.matches(cx).0.into_iter().next()
            {
                this.session = Some(first);
            }
            cx.notify();
        }));
        let time_zones = crate::util::time_zones();
        let saved = store.read(cx).prefs.schedule_timezone.clone();
        let timezone = if time_zones.contains(&saved) {
            saved
        } else {
            crate::util::system_time_zone()
        };
        Self {
            store,
            owner,
            agent,
            existing: session.is_some(),
            session,
            workspace_input,
            workspace,
            search,
            prompt,
            minutes,
            runs,
            steps,
            cost,
            frequency: Frequency::Hourly,
            timezone,
            time_zones,
            timezone_scroll: ScrollHandle::new(),
            pills: PillMenus::default(),
            submitting: false,
            error: None,
            _subs: subs,
        }
    }

    fn interval(&self, cx: &App) -> Result<u64, &'static str> {
        match self.frequency {
            Frequency::Hourly => Ok(3600),
            Frequency::Minutes => interval_seconds(&self.minutes.read(cx).value()),
            _ => Err("This schedule type is not available yet."),
        }
    }

    fn limit(input: &Entity<InputState>, cx: &App) -> Result<Option<u32>, &'static str> {
        positive_limit(&input.read(cx).value()).map_err(|()| "Enter a whole number above zero.")
    }

    fn max_cost(&self, cx: &App) -> Result<Option<f64>, &'static str> {
        let cost = self.cost.read(cx).value();
        let cost = cost.trim();
        if cost.is_empty() {
            return Ok(None);
        }
        cost.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n > 0.)
            .map(Some)
            .ok_or("Enter an amount above zero.")
    }

    fn request(&self, cx: &App) -> Result<NewSchedule, &'static str> {
        let prompt = self.prompt.read(cx).value();
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("Enter the prompt to run.");
        }
        let target = if self.existing {
            let session = self
                .session
                .as_ref()
                .ok_or("Choose a session for this automation.")?;
            ScheduleTarget::Session {
                id: session.id.clone(),
                node: session.node.clone(),
            }
        } else {
            let workspace = self.workspace_input.read(cx).value();
            let path = workspace.trim();
            if path.is_empty() {
                return Err("Enter a workspace path on the agent machine.");
            }
            ScheduleTarget::Workspace(path.to_owned())
        };
        Ok(NewSchedule {
            target,
            prompt: prompt.to_owned(),
            interval_seconds: self.interval(cx)?,
            max_runs: Self::limit(&self.runs, cx)?,
            max_steps: Self::limit(&self.steps, cx)?,
            max_cost_usd: self.max_cost(cx)?,
        })
    }

    /// Shown next to Limits: the limits set, or that one is invalid.
    fn limits_summary(&self, cx: &App) -> Option<(String, bool)> {
        let (runs, steps, cost) = (
            Self::limit(&self.runs, cx),
            Self::limit(&self.steps, cx),
            self.max_cost(cx),
        );
        let (Ok(runs), Ok(steps), Ok(cost)) = (runs, steps, cost) else {
            return Some(("Check the values".into(), true));
        };
        let plural =
            |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let parts: Vec<String> = [
            runs.map(|n| plural(n, "run", "runs")),
            steps.map(|n| plural(n, "step", "steps")),
            cost.map(|c| format!("${c}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| (format!("Max {}", parts.join(" · ")), false))
    }

    /// Remove every limit (no confirmation: the fields are simply emptied).
    fn clear_limits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.runs, &self.steps, &self.cost] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Escape: an open menu first, then the dialog unless it is submitting.
    /// Returns whether the dialog stays.
    fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pills.close() {
            cx.notify();
            return true;
        }
        self.submitting
    }

    fn close(&self, cx: &mut App) {
        let _ = self.owner.update(cx, |app, cx| {
            app.automations.form = None;
            cx.notify();
        });
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.submitting {
            return;
        }
        let request = match self.request(cx) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
                return;
            }
        };
        self.submitting = true;
        self.pills.close();
        self.error = None;
        self.workspace.update(cx, |f, cx| f.set_disabled(true, cx));
        cx.notify();
        let agent = self.agent.clone();
        let task = self
            .store
            .update(cx, |s, cx| s.create_schedule(&agent, request, cx));
        cx.spawn(async move |form, cx| {
            let result = task.await;
            let _ = form.update(cx, |this, cx| {
                this.submitting = false;
                this.workspace.update(cx, |f, cx| f.set_disabled(false, cx));
                match result {
                    Ok(()) => {
                        let _ = this.owner.update(cx, |app, cx| {
                            app.automations.form = None;
                            app.automations.agent = Some(agent);
                            app.go(Page::Automations, cx);
                        });
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Sessions of the agent that can be targeted, newest first, matching
    /// the search; and how many matched before the cap.
    fn matches(&self, cx: &App) -> (Vec<SessionChoice>, usize) {
        let query = self.search.read(cx).value().to_lowercase();
        let store = self.store.read(cx);
        let Some(rt) = store.runtime(&self.agent) else {
            return (Vec::new(), 0);
        };
        let mut found: Vec<&SessionSummary> = rt
            .sessions
            .iter()
            .filter(|s| s.root)
            .filter(|s| {
                s.remote.as_ref().is_none_or(|remote| {
                    rt.supports_remote_schedules()
                        && remote.connected
                        && store.prefs.shows_node(Some(&remote.node_id))
                })
            })
            .filter(|s| {
                let (title, cwd) = (s.title.to_lowercase(), s.cwd.to_lowercase());
                query
                    .split_whitespace()
                    .all(|t| title.contains(t) || s.id.contains(t) || cwd.contains(t))
            })
            .collect();
        found.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let total = found.len();
        let choices = found
            .into_iter()
            .take(PICKER_MAX)
            .map(SessionChoice::from)
            .collect();
        (choices, total)
    }

    /// A single-line field on the dialog fill (desktop `.input-shell` in
    /// `.app-dialog`): no border until focused, then a faint one.
    fn shell(&self, input: &Entity<InputState>, window: &Window, p: &Palette, cx: &App) -> Div {
        let focused = input.read(cx).focus_handle(cx).is_focused(window);
        kit::field_shell(focused, p).h(px(34.))
    }

    fn session_picker(&self, window: &Window, p: &Palette, cx: &mut Context<Self>) -> Div {
        let (choices, total) = self.matches(cx);
        let searching = !self.search.read(cx).value().trim().is_empty();
        let (hover, selected_bg) = (kit::flat_fill(p).1, p.accent_dim);
        let rows: Vec<AnyElement> = choices
            .into_iter()
            .map(|c| {
                let selected = self
                    .session
                    .as_ref()
                    .is_some_and(|s| s.id == c.id && s.node == c.node);
                let key = format!("{}:{}", c.node.as_deref().unwrap_or_default(), c.id);
                h_flex()
                    .id(kit::keyed("schedule-session", key))
                    .flex_none()
                    .gap_2()
                    .px_2p5()
                    .py_1p5()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .map(|el| {
                        if selected {
                            el.bg(selected_bg)
                        } else {
                            el.hover(move |s| s.bg(hover))
                        }
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .when(selected, |el| el.font_medium())
                                    .child(c.title.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .truncate()
                                    .child(c.meta.clone()),
                            ),
                    )
                    .when(selected, |el| {
                        el.child(kit::icon(IconName::Check, 14., p.accent))
                    })
                    .when(!self.submitting, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.session = Some(c.clone());
                            cx.notify();
                        }))
                    })
                    .into_any_element()
            })
            .collect();
        let note = |text: String| {
            div()
                .px_2p5()
                .py_2()
                .text_xs()
                .text_color(p.muted)
                .child(text)
        };
        let empty = rows.is_empty();
        let shown = rows.len();
        v_flex()
            .gap_1p5()
            .child(
                self.shell(&self.search, window, p, cx)
                    .child(kit::icon(IconName::Search, 14., p.dim))
                    .child(
                        Input::new(&self.search)
                            .appearance(false)
                            .aria_label("Search sessions")
                            .disabled(self.submitting)
                            .flex_1(),
                    ),
            )
            .child(
                v_flex()
                    .id("schedule-session-list")
                    .max_h(px(212.))
                    .overflow_y_scroll()
                    .p_1()
                    .gap_0p5()
                    .rounded(px(8.))
                    .bg(kit::flat_fill(p).0)
                    .children(rows)
                    .when(empty, |el| {
                        el.child(note(if searching {
                            "No sessions match the search.".into()
                        } else {
                            "No sessions for this agent.".into()
                        }))
                    })
                    .when(total > shown, |el| {
                        el.child(note(format!(
                            "Showing the first {shown} of {total}. Search to narrow the list."
                        )))
                    }),
            )
    }

    /// Runs: frequency switch with the time zone beside it, then the
    /// interval sentence or a summary.
    fn render_runs(&self, window: &Window, p: &Palette, cx: &mut Context<Self>) -> Div {
        let busy = self.submitting;
        let frequency = kit::segment_bar(p).children(Frequency::ALL.map(|(f, text)| {
            kit::segment(("schedule-frequency", f as usize), self.frequency == f, p)
                .label(text)
                .map(|b| {
                    if f.available() {
                        b.inactive(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.frequency = f;
                                cx.notify();
                            }))
                    } else {
                        b.inactive(true)
                    }
                })
        }));
        let timezone = pill_select(
            PillSelect {
                id: TIMEZONE_MENU,
                icon: IconName::Globe,
                options: self
                    .time_zones
                    .iter()
                    .map(|z| (z.clone(), z.clone()))
                    .collect(),
                selected: self.timezone.clone(),
                open: self.pills.is_open(TIMEZONE_MENU),
                enabled: !busy,
                quiet: true,
                align_right: true,
                menu_w: 260.,
                scroll: Some(self.timezone_scroll.clone()),
            },
            |this: &mut Self, zone, cx| {
                this.store
                    .update(cx, |s, _| s.set_schedule_timezone(zone.clone()));
                this.timezone = zone;
            },
            p,
            cx,
        );

        let note = match (self.frequency, self.interval(cx)) {
            (Frequency::Minutes, Ok(_)) => None,
            (_, Ok(seconds)) => Some((interval_label(seconds), p.dim)),
            (_, Err(error)) => Some((error.to_owned(), p.danger)),
        };
        let interval = (self.frequency == Frequency::Minutes).then(|| {
            let focused = self.minutes.read(cx).focus_handle(cx).is_focused(window);
            let (fill, field) = kit::flat_fill(p);
            // "Every [ 30 ] minutes": one sentence on a faint surface.
            h_flex()
                .gap_2()
                .pl(px(12.8))
                .pr(px(6.4))
                .py(px(4.8))
                .rounded(px(8.))
                .bg(fill)
                .text_size(kit::rpx(13.))
                .text_color(p.muted)
                .child("Every")
                .child(
                    div()
                        .w(px(64.))
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .bg(field)
                        .border_1()
                        .border_color(if focused {
                            kit::field_focus_border(p)
                        } else {
                            transparent_black()
                        })
                        .font_semibold()
                        .text_color(p.text)
                        .child(
                            Input::new(&self.minutes)
                                .appearance(false)
                                .aria_label("Minutes between runs")
                                .disabled(busy),
                        ),
                )
                .child("minutes")
        });
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(field_label("Runs"))
                    .child(help_icon(
                        "schedule-runs-help",
                        "Daily, Weekdays and Custom need cron scheduling, which the agent does \
                         not support yet. Minutes and Hourly work today.",
                        p,
                    )),
            )
            .child(
                h_flex()
                    .justify_between()
                    .gap_3()
                    .child(frequency)
                    .child(timezone),
            )
            .children(interval.map(|el| h_flex().child(el)))
            .children(note.map(|(text, tone)| field_help(text, tone)))
    }

    /// The Limits popover over the footer's Limits button.
    fn render_limits(&self, window: &Window, p: &Palette, cx: &mut Context<Self>) -> Div {
        let open = self.pills.is_open(LIMITS_MENU);
        let summary = self.limits_summary(cx);
        let (_, hover) = kit::flat_fill(p);
        let busy = self.submitting;
        // Clears every limit at once; nothing is lost that is not on screen.
        let clear = (summary.is_some() && !busy).then(|| {
            let (muted, text) = (p.dim, p.text);
            div()
                .id("schedule-limits-clear")
                .flex_none()
                .size(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .text_color(muted)
                .hover(move |s| s.bg(hover).text_color(text))
                .child(Icon::new(IconName::X).size(px(12.)))
                .tooltip(|window, cx| kit::wrap_tip("Clear limits").build(window, cx))
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.clear_limits(window, cx);
                }))
        });
        let trigger = h_flex()
            .id(LIMITS_MENU)
            .h(px(30.))
            .px(px(9.6))
            .gap(px(6.4))
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(kit::rpx(12.5))
            .font_weight(FontWeight(560.))
            .text_color(if open { p.text } else { p.muted })
            .when(open, |el| el.bg(hover))
            .hover(move |s| s.bg(hover))
            .child(kit::icon(IconName::SlidersHorizontal, 14., p.muted))
            .child("Limits")
            .children(summary.map(|(text, invalid)| {
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight(450.))
                    .text_color(if invalid { p.danger } else { p.dim })
                    .child(text)
            }))
            .when_some(clear, |el, clear| el.pr(px(5.)).child(clear))
            .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                toggle_pill(LIMITS_MENU, ev, this);
                cx.notify();
            }));
        let panel = open.then(|| {
            let row = |id: &'static str,
                       label: &'static str,
                       help: &'static str,
                       input: &Entity<InputState>,
                       error: Option<&'static str>| {
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_4()
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1p5()
                                    .text_size(kit::rpx(13.))
                                    .child(label)
                                    .child(help_icon(id, help, p)),
                            )
                            .child(
                                self.shell(input, window, p, cx)
                                    .w(px(152.))
                                    .h(px(32.))
                                    .child(
                                        Input::new(input)
                                            .appearance(false)
                                            .aria_label(label)
                                            .disabled(self.submitting)
                                            .flex_1(),
                                    ),
                            ),
                    )
                    .children(
                        error.map(|e| h_flex().justify_end().child(field_help(e.into(), p.danger))),
                    )
            };
            deferred(
                v_flex()
                    .id("schedule-limits-panel")
                    .absolute()
                    .bottom(px(38.))
                    .left_0()
                    .w(px(384.))
                    .gap(px(9.6))
                    .px_4()
                    .py(px(14.4))
                    .rounded(px(12.))
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        if this.pills.is_open(LIMITS_MENU) {
                            this.pills.open = None;
                            this.pills.dismissed.record(LIMITS_MENU, ev);
                            cx.notify();
                        }
                    }))
                    .child(row(
                        "schedule-max-runs-help",
                        "Max runs",
                        "Stop the automation after it has run this many times. Leave empty to \
                         keep it running until you pause or delete it.",
                        &self.runs,
                        Self::limit(&self.runs, cx).err(),
                    ))
                    .child(row(
                        "schedule-max-steps-help",
                        "Max steps per run",
                        "A step is one model call. This caps how many steps a single run may \
                         take, so a run that loops cannot go on forever. Leave empty for no cap.",
                        &self.steps,
                        Self::limit(&self.steps, cx).err(),
                    ))
                    .child(row(
                        "schedule-max-cost-help",
                        "Max cost per run (USD)",
                        "The most a single run may cost, in US dollars. Leave empty for no cap.",
                        &self.cost,
                        self.max_cost(cx).err(),
                    )),
            )
            .with_priority(4)
        });
        div()
            .relative()
            .min_w_0()
            .flex_shrink(1.)
            .child(trigger)
            .children(panel)
    }
}

fn field_label(text: &'static str) -> Div {
    div().text_sm().font_medium().child(text)
}

fn field_help(text: String, tone: Hsla) -> Div {
    div().text_xs().text_color(tone).child(text)
}

impl Render for ScheduleForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let busy = self.submitting;
        let can_submit = !busy && self.request(cx).is_ok();
        let choices = schedule_agents(self.store.read(cx), Some(CREATE));

        let prompt_focused = self.prompt.read(cx).focus_handle(cx).is_focused(window);
        let prompt = v_flex().gap_1p5().child(field_label("Prompt")).child(
            kit::field_shell(prompt_focused, &p)
                .items_start()
                .py(px(8.))
                .child(
                    Textarea::new(&self.prompt)
                        .appearance(false)
                        .aria_label("Prompt")
                        .disabled(busy)
                        .flex_1(),
                ),
        );

        // ---- Where
        let target = kit::segment_bar(&p).children(
            [
                (false, IconName::Plus, "New session"),
                (true, IconName::Link, "Existing session"),
            ]
            .map(|(existing, icon, text)| {
                kit::segment(
                    ("schedule-target", usize::from(existing)),
                    self.existing == existing,
                    &p,
                )
                .icon(icon)
                .label(text)
                .inactive(busy)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.existing = existing;
                    cx.notify();
                }))
            }),
        );
        let place = v_flex()
            .gap_1p5()
            .child(field_label("Where"))
            .child(h_flex().child(target))
            .child(if self.existing {
                self.session_picker(window, &p, cx).into_any_element()
            } else {
                self.workspace.clone().into_any_element()
            })
            .child(field_help(
                if self.existing {
                    "Every run continues this session."
                } else {
                    "Creates a dedicated session in this workspace."
                }
                .into(),
                p.dim,
            ));

        // ---- Agent, when there is a choice
        let agent = (choices.len() > 1).then(|| {
            v_flex()
                .gap_1p5()
                .child(field_label("Agent"))
                .child(h_flex().child(pill_select(
                    PillSelect {
                        id: AGENT_MENU,
                        icon: IconName::Bot,
                        options: choices,
                        selected: self.agent.clone(),
                        open: self.pills.is_open(AGENT_MENU),
                        enabled: !busy,
                        quiet: false,
                        align_right: false,
                        menu_w: 240.,
                        scroll: None,
                    },
                    |this: &mut Self, id, cx| {
                        this.agent = id.clone();
                        this.session = None;
                        this.workspace.update(cx, |f, cx| f.set_agent(Some(id), cx));
                    },
                    &p,
                    cx,
                )))
        });

        let error = self.error.clone().map(|error| {
            h_flex()
                .items_start()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(px(10.))
                .bg(p.danger.opacity(0.1))
                .text_sm()
                .child(
                    div()
                        .pt(px(2.))
                        .child(kit::icon(IconName::CircleAlert, 15., p.danger)),
                )
                .child(div().flex_1().min_w_0().child(error.clone()))
                .child(kit::copy_button(
                    "schedule-error-copy",
                    error,
                    "Copy details",
                ))
                .child(
                    kit::icon_button("schedule-error-dismiss", IconName::X, "Dismiss")
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                )
        });

        // Title and footer stay put; a tall form scrolls between them.
        let body_max = (window.viewport_size().height - px(240.)).max(px(160.));
        let runs = self.render_runs(window, &p, cx);
        let limits = self.render_limits(window, &p, cx);
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .child(div().text_lg().font_semibold().child("New automation"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(p.muted)
                            .child("Run a prompt on a schedule."),
                    ),
            )
            .child(
                v_flex()
                    .id("automation-form-body")
                    .max_h(body_max)
                    .overflow_y_scroll()
                    .gap_4()
                    .child(prompt)
                    .child(runs)
                    .child(place)
                    .children(agent)
                    .children(error),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(limits)
                    .child(div().flex_1())
                    .child(
                        kit::flat_btn("cancel-schedule", !busy, true, &p)
                            .child("Cancel")
                            .when(!busy, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.close(cx)))
                            }),
                    )
                    .child(
                        kit::soft_accent_btn("create-schedule", can_submit || busy, &p)
                            .when(busy, |el| {
                                el.child(kit::spinner("create-schedule-spinner", 13., p.accent))
                            })
                            .child(if busy {
                                "Creating…"
                            } else {
                                "Create automation"
                            })
                            .when(can_submit, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                            }),
                    ),
            )
    }
}

fn interval_seconds(input: &str) -> Result<u64, &'static str> {
    input
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .and_then(|n| n.checked_mul(60))
        .filter(|n| i64::try_from(*n).is_ok())
        .ok_or("Enter a whole number of minutes above zero.")
}

fn positive_limit(input: &str) -> Result<Option<u32>, ()> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    input
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or(())
}

#[cfg(test)]
mod tests {
    use super::{
        Page, QmtApp, compact_path, describe_cron, duration_label, interval_seconds,
        positive_limit, relative_to, trigger_label,
    };
    use serde_json::json;

    #[gpui_kit::test]
    fn create_dialog_rejects_incomplete_input_and_returns_to_catalog(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| crate::store::automations::schedule_ui_fixture());
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update(|cx| app.update(cx, |app, cx| app.go(Page::Automations, cx)));
        // New automation is an app flat button (a div), which the test
        // observer does not register; open the dialog as it does.
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            app.update(cx, |app, cx| app.open_automation_dialog(false, window, cx));
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find_by_label("Prompt").visible());
            // Incomplete input cannot be submitted; the form stays open.
            let form = app.read(cx).automations.form.clone().unwrap();
            form.update(cx, |form, cx| form.submit(cx));
            assert!(!form.read(cx).submitting);
            // Clear limits empties every limit at once.
            form.update(cx, |form, cx| {
                for input in [&form.runs, &form.steps, &form.cost] {
                    input.update(cx, |s, cx| s.set_value("0", window, cx));
                }
                assert_eq!(form.limits_summary(cx).map(|(_, bad)| bad), Some(true));
                form.clear_limits(window, cx);
                assert!(form.limits_summary(cx).is_none());
            });
            window.render_frame(cx);
            assert!(window.find_by_label("Prompt").visible());
            window.press("escape", cx);
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(app.read(cx).automations.form.is_none());
            assert!(window.find("refresh-automations").visible());
        })
        .unwrap();
    }

    #[test]
    fn validates_interval_and_optional_limits() {
        assert_eq!(interval_seconds(" 30 "), Ok(1800));
        for value in ["", "0", "2.5", "-1", "18446744073709551615"] {
            assert!(interval_seconds(value).is_err());
        }
        assert_eq!(positive_limit(" "), Ok(None));
        assert_eq!(positive_limit("8"), Ok(Some(8)));
        assert_eq!(positive_limit("0"), Err(()));
    }

    #[test]
    fn formats_supported_and_agent_created_triggers() {
        assert_eq!(
            trigger_label(&json!({"type":"interval","seconds":3600})),
            "Every hour"
        );
        assert_eq!(
            trigger_label(&json!({"type":"interval","seconds":600})),
            "Every 10 minutes"
        );
        assert_eq!(
            trigger_label(&json!({"type":"interval","seconds":172800})),
            "Every 2 days"
        );
        assert_eq!(
            trigger_label(&json!({"type":"event_driven"})),
            "Event-triggered"
        );
        assert_eq!(describe_cron("30 9 * * 1-5"), "Weekdays at 09:30");
        assert_eq!(describe_cron("0 7 * * *"), "Every day at 07:00");
        assert_eq!(describe_cron("*/5 * * * *"), "Cron: */5 * * * *");
    }

    #[test]
    fn formats_times_paths_and_durations() {
        let minute = 60_000.;
        assert_eq!(relative_to(5. * minute, 0.).as_deref(), Some("in 5m"));
        assert_eq!(
            relative_to(0., 3. * 60. * minute).as_deref(),
            Some("3h ago")
        );
        assert_eq!(
            relative_to(2. * 1440. * minute, 0.).as_deref(),
            Some("in 2d")
        );
        assert_eq!(compact_path("/home/me/src/app"), "src/app");
        assert_eq!(compact_path("/srv"), "/srv");
        assert_eq!(duration_label(0), "None");
        assert_eq!(duration_label(5400), "1h 30m");
    }
}
