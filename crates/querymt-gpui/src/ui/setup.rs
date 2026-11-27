//! First-run setup (desktop `routes/setup` and `SetupChecklist`): a short
//! wizard over appearance, the agent, providers and the launch model, the
//! "Finish setup" checklist on Start, and the first-run redirect.
//!
//! The steps reuse the app's own pieces: Settings' appearance controls and
//! provider list, the Add/Edit agent dialog and the model picker's rows.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::settings::SettingsTab;
use super::{Page, QmtApp, kit};
use crate::store::ConnState;
use crate::store::setup::{CheckAction, CheckId, CheckStatus, Readiness};
use crate::theme::{Palette, palette};

/// A first run decides once agents settle, or after this long at the latest.
const FIRST_RUN_SETTLE_CAP: Duration = Duration::from_secs(10);
const SETUP_W: f32 = 640.;
/// Height of the model list in the Model step.
const MODEL_LIST_H: f32 = 360.;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Step {
    Welcome,
    Appearance,
    Agent,
    Providers,
    Model,
    Done,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Appearance => "Appearance",
            Self::Agent => "Agent",
            Self::Providers => "Providers",
            Self::Model => "Model",
            Self::Done => "Done",
        }
    }
}

#[derive(Default)]
pub(super) struct SetupState {
    step: usize,
    /// Furthest step reached; later ones cannot be jumped to.
    visited: usize,
    skipped: Vec<Step>,
    first_run_task: Option<Task<()>>,
}

/// Ready / skipped / missing, for the Done summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Summary {
    Ready,
    Skipped,
    Missing,
}

impl QmtApp {
    /// The wizard's steps; an agent that serves this app (the browser build
    /// on its own server) needs no Agent step.
    fn setup_steps(&self, cx: &App) -> Vec<Step> {
        let store = self.store.read(cx);
        let embedded = !store.configs.is_empty() && store.configs.iter().all(|c| c.host_managed);
        [
            Step::Welcome,
            Step::Appearance,
            Step::Agent,
            Step::Providers,
            Step::Model,
            Step::Done,
        ]
        .into_iter()
        .filter(|s| !(embedded && *s == Step::Agent))
        .collect()
    }

    fn setup_step(&self, cx: &App) -> Step {
        let steps = self.setup_steps(cx);
        steps[self.setup.step.min(steps.len() - 1)]
    }

    /// Open setup, at `at` when given (and reachable from the start).
    pub(super) fn open_setup(&mut self, at: Option<Step>, cx: &mut Context<Self>) {
        let index = at
            .and_then(|s| self.setup_steps(cx).iter().position(|x| *x == s))
            .unwrap_or(0);
        self.setup.step = index;
        self.setup.visited = index;
        self.setup.skipped.clear();
        self.close_palette(cx);
        self.go(Page::Setup, cx);
        if let Some(agent) = self.store.read(cx).primary_agent() {
            self.store.update(cx, |s, cx| s.refresh_extras(&agent, cx));
        }
    }

    fn setup_go(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.setup_steps(cx).len() - 1;
        self.setup.step = index.min(last);
        self.setup.visited = self.setup.visited.max(self.setup.step);
        self.model_picker
            .update(cx, |picker, cx| picker.clear_query(window, cx));
        cx.notify();
    }

    fn setup_next(&mut self, skip: bool, window: &mut Window, cx: &mut Context<Self>) {
        let step = self.setup_step(cx);
        self.setup.skipped.retain(|s| *s != step);
        if skip {
            self.setup.skipped.push(step);
        }
        self.setup_go(self.setup.step + 1, window, cx);
    }

    /// Leave setup for Start; `skipped` records that it was not finished.
    fn leave_setup(&mut self, skipped: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, _| s.finish_onboarding(skipped));
        self.go(Page::Start, cx);
        if !skipped {
            self.prompt.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    /// On a first run, once agents settle (or the cap passes): record setup
    /// as done when the app is already usable, otherwise open it from Start.
    pub(super) fn watch_first_run(&mut self, cx: &mut Context<Self>) {
        if !self.store.read(cx).first_run {
            return;
        }
        self.setup.first_run_task = Some(cx.spawn(async move |this, cx| {
            // Polls on the executor's timer (not the wall clock), so the cap
            // is honoured in tests too.
            const POLL: Duration = Duration::from_millis(200);
            for _ in 0..FIRST_RUN_SETTLE_CAP.as_millis() / POLL.as_millis() {
                let settled = this
                    .read_with(cx, |this, cx| this.store.read(cx).settled())
                    .unwrap_or(true);
                if settled {
                    break;
                }
                cx.background_executor().timer(POLL).await;
            }
            let _ = this.update(cx, |this, cx| {
                if !this.store.read(cx).first_run {
                    return;
                }
                if this.store.read(cx).readiness().complete() {
                    this.store.update(cx, |s, _| s.finish_onboarding(false));
                } else if this.page == Page::Start {
                    this.open_setup(None, cx);
                }
            });
        }));
    }

    /// Resolve an open readiness check from the Start checklist.
    fn run_check(&mut self, action: CheckAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            CheckAction::RetryAgent(id) => self.store.update(cx, |s, cx| s.connect(&id, cx)),
            CheckAction::AddAgent => self.open_add_dialog(window, cx),
            CheckAction::OpenProviders => {
                self.settings_tab = SettingsTab::Providers;
                self.go(Page::Settings, cx);
            }
            CheckAction::RefreshProviders(id) => {
                self.store.update(cx, |s, cx| s.refresh_extras(&id, cx))
            }
            CheckAction::RefreshModels(id) => {
                self.store.update(cx, |s, cx| s.refresh_models(&id, cx))
            }
            CheckAction::ChooseModel => self.open_model_picker(window, cx),
        }
    }

    /// "Finish setup" on Start (desktop `SetupChecklist`): each open check
    /// with its reason and the action that resolves it.
    pub(super) fn setup_checklist(&self, p: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let store = self.store.read(cx);
        if !store.settled() {
            return None;
        }
        let readiness = store.readiness();
        if readiness.complete() {
            return None;
        }
        let rows: Vec<AnyElement> =
            readiness
                .open()
                .cloned()
                .enumerate()
                .map(|(ix, check)| {
                    let tone = match check.status {
                        CheckStatus::Warning => p.warn,
                        CheckStatus::Blocked => p.danger,
                        _ => p.dim,
                    };
                    let action = check
                        .action
                        .clone()
                        .filter(|_| check.status != CheckStatus::Pending);
                    h_flex()
                        .gap_3()
                        .px_4()
                        .py(px(10.))
                        .when(ix > 0, |el| el.border_t_1().border_color(kit::subtle(p).0))
                        .child(kit::dot(tone))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_semibold().child(check.title))
                                .children(check.reason.clone().map(|reason| {
                                    div().text_xs().text_color(p.muted).child(reason)
                                })),
                        )
                        .children(action.map(|action| {
                            kit::flat_btn(kit::keyed("setup-check", check.title), true, true, p)
                                .child(action_label(&action))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.run_check(action.clone(), window, cx)
                                }))
                        }))
                        .into_any_element()
                })
                .collect();
        Some(
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    h_flex()
                        .px_1()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_semibold()
                                .text_color(p.muted)
                                .child("Finish setup"),
                        )
                        .child(
                            kit::flat_btn("run-setup-start", true, true, p)
                                .child("Run setup")
                                .on_click(cx.listener(|this, _, _, cx| this.open_setup(None, cx))),
                        ),
                )
                .child(kit::card(p).children(rows)),
        )
    }

    // ---- page ---------------------------------------------------------------

    pub(super) fn render_setup(&mut self, cx: &mut Context<Self>) -> Div {
        let p = *palette(cx);
        let steps = self.setup_steps(cx);
        let index = self.setup.step.min(steps.len() - 1);
        let step = steps[index];
        let readiness = self.store.read(cx).readiness();

        let progress = h_flex().justify_center().flex_wrap().gap(px(3.)).children(
            steps.iter().enumerate().map(|(ix, s)| {
                let (done, current, reachable) =
                    (ix < index, ix == index, ix <= self.setup.visited);
                let hover = kit::flat_fill(&p).1;
                h_flex()
                    .id(("setup-step", ix))
                    .gap(px(6.))
                    .px(px(9.))
                    .py(px(5.))
                    .rounded(px(8.))
                    .text_size(px(12.5))
                    .text_color(if current {
                        p.text
                    } else if done {
                        p.muted
                    } else {
                        p.dim
                    })
                    .when(current, |el| el.bg(hover).font_semibold())
                    .when(reachable && !current, |el| {
                        el.cursor_pointer().hover(move |s| s.bg(hover)).on_click(
                            cx.listener(move |this, _, window, cx| this.setup_go(ix, window, cx)),
                        )
                    })
                    .child(
                        div()
                            .w(px(16.))
                            .flex()
                            .justify_center()
                            .text_size(px(11.5))
                            .child(if done {
                                kit::icon(IconName::Check, 13., p.accent).into_any_element()
                            } else {
                                (ix + 1).to_string().into_any_element()
                            }),
                    )
                    .child(s.label())
            }),
        );

        let body = match step {
            Step::Welcome => hero(
                kit::boot_mark(120., &p).into_any_element(),
                "Welcome to QueryMT",
                "Let's get you ready to start your first session. We'll set the look of the \
                 app, check the agent, connect a model provider and pick a model. It takes \
                 about a minute, and you can skip any step.",
                &p,
            ),
            Step::Appearance => v_flex()
                .gap_4()
                .child(header(
                    "Appearance",
                    "Pick a theme and accent color. Changes apply right away, and you can \
                     change them later in Settings, Appearance.",
                    &p,
                ))
                .child(self.appearance_controls(&p, cx)),
            Step::Agent => self.setup_agent(&readiness, &p, cx),
            Step::Providers => self.setup_providers(&readiness, &p, cx),
            Step::Model => self.setup_model(&readiness, &p, cx),
            Step::Done => self.setup_done(&readiness, &p),
        };

        let can_continue = match step {
            Step::Agent => readiness.check(CheckId::Agent).status == CheckStatus::Ok,
            Step::Providers => readiness.check(CheckId::Providers).done(),
            Step::Model => readiness.check(CheckId::Model).status == CheckStatus::Ok,
            _ => true,
        };
        let back = |cx: &Context<Self>| {
            kit::flat_btn("setup-back", true, true, &p)
                .child("Back")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.setup_go(index.saturating_sub(1), window, cx)
                }))
        };
        let footer =
            match step {
                Step::Welcome => h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        kit::flat_btn("setup-skip-all", true, true, &p)
                            .child("Skip setup")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.leave_setup(true, window, cx)
                            })),
                    )
                    .child(
                        kit::accent_btn("setup-start", true, true, &p)
                            .child("Get started")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.setup_next(false, window, cx)
                            })),
                    ),
                Step::Done => h_flex()
                    .justify_end()
                    .gap_2()
                    .child(back(cx))
                    .child(div().flex_1())
                    .child(
                        kit::accent_btn("setup-finish", true, true, &p)
                            .child("Start using QueryMT")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.leave_setup(false, window, cx)
                            })),
                    ),
                _ => {
                    let quiet = p.dim;
                    let text = p.text;
                    h_flex()
                        .gap_2()
                        .child(back(cx))
                        .child(div().flex_1())
                        .child(
                            div()
                                .id("setup-skip-all")
                                .px_1p5()
                                .cursor_pointer()
                                .text_size(px(12.5))
                                .text_color(quiet)
                                .hover(move |s| s.text_color(text))
                                .child("Skip setup")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.leave_setup(true, window, cx)
                                })),
                        )
                        .child(
                            kit::flat_btn("setup-skip", true, true, &p)
                                .child("Skip")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.setup_next(true, window, cx)
                                })),
                        )
                        .child(
                            kit::accent_btn("setup-continue", can_continue, true, &p)
                                .child("Continue")
                                .when(can_continue, |el| {
                                    el.on_click(cx.listener(|this, _, window, cx| {
                                        this.setup_next(false, window, cx)
                                    }))
                                }),
                        )
                }
            };

        // Progress, card and actions sit together in the middle of the window.
        div()
            .size_full()
            .min_h_full()
            .flex()
            .items_center()
            .justify_center()
            .px_4()
            .py_10()
            .child(
                v_flex()
                    .w(px(SETUP_W))
                    .max_w_full()
                    .gap_4()
                    .child(progress)
                    .child(
                        v_flex()
                            .gap_4()
                            .px_6()
                            .pt_5()
                            .pb(px(22.))
                            .rounded(px(14.))
                            .bg(p.surface)
                            .border_1()
                            .border_color(p.border)
                            .child(body),
                    )
                    .child(footer),
            )
    }

    fn setup_agent(&self, readiness: &Readiness, p: &Palette, cx: &mut Context<Self>) -> Div {
        let store = self.store.read(cx);
        let agent = readiness.check(CheckId::Agent).clone();
        // Startup log from readiness, never from timers (desktop boot log).
        let log = [
            ("Starting agent", readiness.check(CheckId::Agent)),
            ("Checking providers", readiness.check(CheckId::Providers)),
            ("Loading models", readiness.check(CheckId::Models)),
        ]
        .into_iter()
        .filter(|(_, c)| c.id == CheckId::Agent || c.status != CheckStatus::NotApplicable)
        .enumerate()
        .map(|(ix, (label, check))| {
            let (mark, color, text): (AnyElement, Hsla, String) = match check.status {
                CheckStatus::Ok | CheckStatus::NotApplicable => (
                    kit::icon(IconName::Check, 13., p.accent).into_any_element(),
                    p.dim,
                    label.into(),
                ),
                CheckStatus::Pending => (
                    kit::spinner(("setup-log", ix), 12., p.muted).into_any_element(),
                    p.text,
                    format!("{label}…"),
                ),
                _ => (
                    kit::icon(IconName::X, 13., p.danger).into_any_element(),
                    p.muted,
                    label.into(),
                ),
            };
            h_flex()
                .gap_2()
                .text_color(color)
                .child(div().w(px(14.)).flex().justify_center().child(mark))
                .child(text)
        });
        let hero = v_flex()
            .items_center()
            .gap_2()
            .child(kit::boot_mark(104., p))
            .child(div().text_xl().font_semibold().child("Agent"))
            .child(
                v_flex()
                    .items_center()
                    .gap_0p5()
                    .font_family(kit::MONO)
                    .text_size(px(11.8))
                    .children(log),
            );

        let detail: Option<AnyElement> = match (&agent.status, &agent.action) {
            (CheckStatus::Ok, _) => readiness.agent.as_deref().and_then(|id| {
                let config = store.config(id)?;
                let rt = store.runtime(id);
                let mut facts = vec![
                    ("Agent", config.name.clone()),
                    ("Connection", config.endpoint_label()),
                ];
                if let Some(version) = rt.and_then(|r| r.agent_version.clone()) {
                    facts.push(("Version", version));
                }
                Some(facts_list(facts, p).into_any_element())
            }),
            (_, Some(CheckAction::AddAgent)) => Some(
                v_flex()
                    .gap_3()
                    .child(note(
                        "No agent is configured yet. Add one to continue: a local command the \
                         app starts for you, or the address of an ACP WebSocket server that is \
                         already running.",
                        p,
                    ))
                    .child(
                        h_flex().child(
                            kit::accent_btn("setup-add-agent", true, true, p)
                                .child(kit::icon(IconName::Plus, 14., p.on_accent))
                                .child("Add agent")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_add_dialog(window, cx)
                                })),
                        ),
                    )
                    .into_any_element(),
            ),
            (CheckStatus::Blocked, Some(CheckAction::RetryAgent(id))) => {
                let connecting = store.state(id) == ConnState::Connecting;
                let config = store.config(id).cloned();
                let id = id.clone();
                Some(
                    error_strip(agent.reason.clone().unwrap_or_default(), p)
                        .child(
                            kit::flat_btn("setup-retry-agent", !connecting, true, p)
                                .when(connecting, |el| {
                                    el.child(kit::spinner("setup-retry-spin", 13., p.muted))
                                })
                                .child("Retry")
                                .when(!connecting, |el| {
                                    el.on_click(cx.listener(move |this, _, _, cx| {
                                        this.store.update(cx, |s, cx| s.connect(&id, cx))
                                    }))
                                }),
                        )
                        .children(config.map(|config| {
                            kit::flat_btn("setup-edit-agent", true, true, p)
                                .child("Edit agent")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_edit_dialog(&config, window, cx)
                                }))
                        }))
                        .into_any_element(),
                )
            }
            _ => None,
        };
        v_flex().gap_4().child(hero).children(detail)
    }

    fn setup_providers(&self, readiness: &Readiness, p: &Palette, cx: &mut Context<Self>) -> Div {
        let providers = readiness.check(CheckId::Providers);
        let agent_ok = readiness.check(CheckId::Agent).status == CheckStatus::Ok;
        let content: AnyElement = match providers.status {
            CheckStatus::NotApplicable => note(
                if agent_ok {
                    "This agent does not manage provider sign-in. Its models are configured \
                     on the agent."
                } else {
                    "Start the agent first to see its providers."
                },
                p,
            )
            .into_any_element(),
            CheckStatus::Pending if !self.has_providers(readiness, cx) => {
                note("Checking providers…", p).into_any_element()
            }
            status => v_flex()
                .gap_3()
                .when(status == CheckStatus::Ok, |el| {
                    el.child(
                        h_flex()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .rounded(px(10.))
                            .bg(p.success.opacity(0.1))
                            .text_sm()
                            .child(kit::icon(IconName::CircleCheck, 15., p.success))
                            .child("A provider is connected. You can add more or continue."),
                    )
                })
                .child(self.settings_providers(true, p, cx))
                .into_any_element(),
        };
        v_flex()
            .gap_4()
            .child(header(
                "Connect a provider",
                "Sign in or add an API key for at least one model provider.",
                p,
            ))
            .child(content)
    }

    fn has_providers(&self, readiness: &Readiness, cx: &App) -> bool {
        let store = self.store.read(cx);
        readiness
            .agent
            .as_deref()
            .and_then(|a| store.runtime(a))
            .is_some_and(|rt| !rt.providers.is_empty())
    }

    fn setup_model(&self, readiness: &Readiness, p: &Palette, cx: &mut Context<Self>) -> Div {
        let head = header(
            "Choose a model",
            "New sessions start with this model. QueryMT remembers it as the last used \
             model for this agent.",
            p,
        );
        let Some(agent) = readiness.agent.clone() else {
            return v_flex()
                .gap_4()
                .child(head)
                .child(note("Start the agent first to see its models.", p));
        };
        let models = readiness.check(CheckId::Models);
        if models.status != CheckStatus::Ok {
            let refreshing = models.status == CheckStatus::Pending;
            let agent2 = agent.clone();
            return v_flex()
                .gap_4()
                .child(head)
                .child(note(
                    models.reason.as_deref().unwrap_or("Loading models…"),
                    p,
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            kit::flat_btn("setup-refresh-models", !refreshing, true, p)
                                .child(if refreshing {
                                    kit::spinner("setup-models-spin", 13., p.muted)
                                        .into_any_element()
                                } else {
                                    kit::icon(IconName::RefreshCw, 14., p.text).into_any_element()
                                })
                                .child("Refresh models")
                                .when(!refreshing, |el| {
                                    el.on_click(cx.listener(move |this, _, _, cx| {
                                        this.store.update(cx, |s, cx| s.refresh_models(&agent2, cx))
                                    }))
                                }),
                        )
                        .child(
                            kit::flat_btn("setup-to-providers", true, true, p)
                                .child("Back to providers")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let at = this
                                        .setup_steps(cx)
                                        .iter()
                                        .position(|s| *s == Step::Providers)
                                        .unwrap_or(0);
                                    this.setup_go(at, window, cx)
                                })),
                        ),
                );
        }
        let rows = self.model_picker.update(cx, |picker, cx| picker.rows(cx));
        v_flex()
            .gap_3()
            .child(head)
            .child(
                self.model_picker
                    .update(cx, |picker, cx| picker.search(&agent, p, cx)),
            )
            .child(
                v_flex()
                    .id("setup-model-list")
                    .h(px(MODEL_LIST_H))
                    .overflow_y_scroll()
                    .track_scroll(&self.model_picker.read(cx).scroll_handle())
                    .children(rows)
                    .map(|el| self.smooth_scroll.region("setup-model-list", el)),
            )
    }

    fn setup_done(&self, readiness: &Readiness, p: &Palette) -> Div {
        let complete = readiness.complete();
        let summary = |id: CheckId| {
            // The Model row also covers the model list: without models
            // there is nothing to pick.
            let done = match id {
                CheckId::Model => {
                    readiness.check(CheckId::Models).done()
                        && readiness.check(CheckId::Model).done()
                }
                id => readiness.check(id).done(),
            };
            let steps: &[Step] = match id {
                CheckId::Agent => &[Step::Agent],
                CheckId::Providers => &[Step::Providers],
                _ => &[Step::Model, Step::Providers],
            };
            if done {
                Summary::Ready
            } else if steps.iter().any(|s| self.setup.skipped.contains(s)) {
                Summary::Skipped
            } else {
                Summary::Missing
            }
        };
        let rows = [
            (CheckId::Agent, "Agent"),
            (CheckId::Providers, "Providers"),
            (CheckId::Model, "Model"),
        ]
        .into_iter()
        .enumerate()
        .map(|(ix, (id, label))| {
            let state = summary(id);
            h_flex()
                .justify_between()
                .gap_4()
                .py(px(9.))
                .text_sm()
                .when(ix > 0, |el| el.border_t_1().border_color(kit::subtle(p).0))
                .child(div().font_medium().child(label))
                .child(match state {
                    Summary::Ready => h_flex()
                        .gap_1p5()
                        .child(kit::icon(IconName::Check, 14., p.accent))
                        .child("Ready"),
                    Summary::Skipped => h_flex().text_color(p.muted).child("Skipped, not set up"),
                    Summary::Missing => h_flex().text_color(p.muted).child("Not set up"),
                })
        });
        v_flex()
            .gap_4()
            .child(hero(
                kit::boot_mark(88., p).into_any_element(),
                if complete {
                    "You're all set"
                } else {
                    "Almost there"
                },
                if complete {
                    "QueryMT is ready for your first session."
                } else {
                    "You can finish the remaining items later from the start screen."
                },
                p,
            ))
            .child(v_flex().children(rows))
    }
}

fn action_label(action: &CheckAction) -> &'static str {
    match action {
        CheckAction::RetryAgent(_) | CheckAction::RefreshProviders(_) => "Retry",
        CheckAction::AddAgent => "Add agent",
        CheckAction::OpenProviders => "Connect provider",
        CheckAction::RefreshModels(_) => "Refresh models",
        CheckAction::ChooseModel => "Choose model",
    }
}

fn header(title: &'static str, description: &'static str, p: &Palette) -> Div {
    v_flex()
        .gap_1()
        .child(div().text_lg().font_semibold().child(title))
        .child(div().text_sm().text_color(p.muted).child(description))
}

fn hero(mark: AnyElement, title: &'static str, description: &'static str, p: &Palette) -> Div {
    v_flex()
        .items_center()
        .gap_2()
        .child(mark)
        .child(div().text_xl().font_semibold().child(title))
        .child(
            div()
                .max_w(px(480.))
                .text_center()
                .text_sm()
                .text_color(p.muted)
                .child(description),
        )
}

fn note(text: &str, p: &Palette) -> Div {
    div()
        .text_sm()
        .text_color(p.muted)
        .child(SharedString::from(text.to_owned()))
}

fn error_strip(message: String, p: &Palette) -> Div {
    h_flex()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(px(10.))
        .bg(p.danger.opacity(0.1))
        .text_sm()
        .child(kit::icon(IconName::TriangleAlert, 15., p.danger))
        .child(div().flex_1().min_w_0().child(message))
}

fn facts_list(facts: Vec<(&'static str, String)>, p: &Palette) -> Div {
    v_flex().children(facts.into_iter().enumerate().map(|(ix, (label, value))| {
        h_flex()
            .justify_between()
            .gap_4()
            .py(px(9.))
            .text_sm()
            .when(ix > 0, |el| el.border_t_1().border_color(kit::subtle(p).0))
            .child(div().font_medium().child(label))
            .child(div().min_w_0().truncate().text_color(p.muted).child(value))
    }))
}

#[cfg(test)]
mod tests {
    use super::{Page, QmtApp, Step};

    #[gpui_kit::test]
    fn first_run_opens_setup_and_skip_records_it(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| {
                let mut store = crate::store::automations::schedule_ui_fixture();
                store.first_run = true;
                store
            });
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        // The fixture's agent is connected but never loads models, so it
        // never settles: the cap decides, and setup is not complete.
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(app.read(cx).page, Page::Start));
        cx.executor()
            .advance_clock(super::FIRST_RUN_SETTLE_CAP + std::time::Duration::from_secs(1));
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(app.read(cx).page, Page::Setup));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                assert_eq!(app.setup_step(cx), Step::Welcome);
                app.setup_next(false, window, cx);
                assert_eq!(app.setup_step(cx), Step::Appearance);
                app.setup_next(true, window, cx);
                assert!(app.setup.skipped.contains(&Step::Appearance));
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| app.leave_setup(true, window, cx));
        })
        .unwrap();
        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(app.page, Page::Start);
            let store = app.store.read(cx);
            assert!(!store.first_run && store.prefs.onboarding.done());
        });
    }

    /// The setup's Appearance step has the same two theme selects and the
    /// Theme swatch as Settings (spec: The theme selects mark the theme in use).
    #[gpui_kit::test]
    fn appearance_step_has_the_theme_selects_and_swatch(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::VisualTestContext;
        use gpui_kit::test::TestWindowExt as _;

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
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.open_setup(Some(Step::Appearance), cx);
                assert_eq!(app.setup_step(cx), Step::Appearance);
            });
            window.render_frame(cx);
        })
        .unwrap();
        cx.update(|cx| assert_eq!(app.read(cx).page, Page::Setup));
        for selector in [
            "theme-select-dark",
            "theme-select-light",
            "accent-theme",
            "accent-blue",
        ] {
            assert!(
                VisualTestContext::from_window(handle, cx)
                    .debug_bounds(selector)
                    .is_some(),
                "{selector} is on the setup page"
            );
        }
    }
}
