//! Delegate routing dialog (desktop `DelegateModelDialog`): the delegates of
//! the open session's profile with the model each uses the next time it
//! starts, then, for one delegate, the model picker's rows, its reasoning
//! effort and "Use profile default". Opened from Session options.

use gpui_kit::assets::IconName;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agents::modal_scrim;
use super::kit::ButtonExt as _;
use super::{QmtApp, kit};
use crate::store::delegates::{Assignment, DelegateState, Orphan, REASONING, Route, RouteChange};
use crate::store::find_model;
use crate::theme::{Palette, palette};

/// Height of the delegate's model list.
const LIST_H: f32 = 340.;

#[derive(Default)]
pub(super) struct DelegateDialog {
    pub open: bool,
    /// Delegate whose route is being chosen; `None`: the list of delegates.
    pub picking: Option<String>,
}

impl QmtApp {
    pub(super) fn open_delegate_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_options_open = false;
        self.delegates = DelegateDialog {
            open: true,
            picking: None,
        };
        self.model_query
            .update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        self.store.update(cx, |s, cx| s.refresh_delegates(cx));
        cx.notify();
    }

    fn pick_delegate(
        &mut self,
        agent: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delegates.picking = agent;
        self.model_query.update(cx, |s, cx| {
            s.set_value(SharedString::default(), window, cx);
            if self.delegates.picking.is_some() {
                s.focus(window, cx);
            }
        });
        self.reset_model_cursor(cx);
        cx.notify();
    }

    /// Escape: back to the list from a delegate, then close. A save in
    /// flight keeps the dialog where it is.
    pub(super) fn close_delegate_dialog(&mut self, cx: &App) -> bool {
        if !self.delegates.open {
            return false;
        }
        let busy = self
            .store
            .read(cx)
            .delegate_state()
            .is_some_and(|s| s.pending.is_some());
        if !busy && self.delegates.picking.take().is_none() {
            self.delegates.open = false;
        }
        true
    }

    /// The model the picker shows as chosen while a delegate is open.
    pub(super) fn delegate_selected_key(&self, cx: &App) -> Option<Option<String>> {
        let agent = self.delegates.picking.as_ref()?;
        let routes = self.store.read(cx).delegate_state()?.routes.as_ref()?;
        Some(
            routes
                .assignments
                .iter()
                .find(|a| &a.agent_id == agent)
                .and_then(|a| a.route.as_ref())
                .map(Route::key),
        )
    }

    /// A model row was picked while a delegate is open: route it there.
    pub(super) fn route_delegate_model(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(agent) = self.delegates.picking.clone() else {
            return false;
        };
        let store = self.store.read(cx);
        let Some(owner) = store.session.as_ref().map(|v| v.agent_id.clone()) else {
            return true;
        };
        let Some(model) = find_model(store.models(&owner), key) else {
            return true;
        };
        let change = RouteChange {
            route: Some(Route::of(model)),
            reasoning: None,
        };
        self.store
            .update(cx, |s, cx| s.set_delegate_route(&agent, change, cx));
        true
    }

    /// Recent models of the open delegate, for the picker's Recent group.
    pub(super) fn delegate_recent_models(&self, cx: &App) -> Option<Vec<crate::store::Model>> {
        let agent = self.delegates.picking.as_ref()?;
        let store = self.store.read(cx);
        let owner = store.session.as_ref()?.agent_id.clone();
        let profile = store.delegate_state()?.routes.as_ref()?.profile_id.clone();
        Some(
            store
                .recent_delegate_models(&owner, &profile, agent)
                .into_iter()
                .cloned()
                .collect(),
        )
    }

    pub(super) fn render_delegate_dialog(
        &self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.delegates.open {
            return None;
        }
        let p = *palette(cx);
        let store = self.store.read(cx);
        let owner = store.session.as_ref()?.agent_id.clone();
        let state = store.delegate_state().cloned().unwrap_or_default();
        let routes = state.routes.clone();
        let models = store.models(&owner).to_vec();
        let reasoning_label = store
            .session
            .as_ref()
            .and_then(|v| v.reasoning_option())
            .map(|r| r.current_label())
            .filter(|l| !l.is_empty());

        let picking = self.delegates.picking.clone().and_then(|agent| {
            let routes = routes.as_ref()?;
            routes
                .assignments
                .iter()
                .find(|a| a.agent_id == agent)
                .cloned()
                .map(Picking::Delegate)
                .or_else(|| {
                    routes
                        .orphans
                        .iter()
                        .find(|o| o.agent_id == agent)
                        .cloned()
                        .map(Picking::Orphan)
                })
        });
        let busy = state.pending.is_some();
        let (title, description) = match &picking {
            Some(Picking::Delegate(a)) => (
                format!("Route {}", a.name),
                "The route applies the next time this delegate starts. Work already running is unchanged."
                    .to_string(),
            ),
            Some(Picking::Orphan(o)) => (
                format!("Route {}", o.agent_id),
                "This delegate was removed from the profile. Clear its saved route to finish cleanup."
                    .to_string(),
            ),
            None => (
                "Delegate routing".to_string(),
                match &routes {
                    Some(r) if r.reasoning_supported => format!(
                        "Choose the model and reasoning each {} delegate uses the next time it starts.",
                        r.profile_id
                    ),
                    Some(r) => format!(
                        "Choose which model each {} delegate uses the next time it starts.",
                        r.profile_id
                    ),
                    None => "Choose delegate routes for the next time they start.".into(),
                },
            ),
        };

        let body = match &picking {
            Some(Picking::Delegate(a)) => {
                self.delegate_picker(&owner, a, &state, reasoning_label.as_deref(), &p, cx)
            }
            Some(Picking::Orphan(o)) => v_flex().child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(px(10.))
                    .bg(p.warn.opacity(0.1))
                    .text_sm()
                    .child(kit::icon(IconName::TriangleAlert, 15., p.warn))
                    .child(
                        v_flex()
                            .child(
                                div()
                                    .font_semibold()
                                    .child("Saved route for a removed delegate"),
                            )
                            .child(div().text_xs().text_color(p.muted).child(route_summary(
                                o.route.as_ref(),
                                o.reasoning.as_deref(),
                                routes.as_ref().is_some_and(|r| r.reasoning_supported),
                                reasoning_label.as_deref(),
                                &models,
                            ))),
                    ),
            ),
            None => self.delegate_list(&owner, &state, reasoning_label.as_deref(), &p, cx),
        };

        let footer = match &picking {
            Some(Picking::Delegate(a)) => {
                let is_default = a.route.is_none();
                let default = a
                    .default_model
                    .as_deref()
                    .map(|id| {
                        models
                            .iter()
                            .find(|m| m.id == id && m.node_id.is_none())
                            .map_or_else(|| id.to_string(), |m| m.label.clone())
                    })
                    .unwrap_or_else(|| "Profile default".into());
                let agent = a.agent_id.clone();
                h_flex()
                    .gap_2()
                    .child(
                        kit::flat_btn("delegate-default", !busy, true, &p)
                            .child(kit::icon(IconName::RotateCcw, 14., p.text))
                            .child(format!("Use profile default · {default}"))
                            .when(is_default, |el| {
                                el.child(kit::icon(IconName::Check, 13., p.accent))
                            })
                            .when(!busy && !is_default, |el| {
                                el.on_click(cx.listener(move |this, _, _, cx| {
                                    let change = RouteChange {
                                        route: None,
                                        reasoning: None,
                                    };
                                    this.store.update(cx, |s, cx| {
                                        s.set_delegate_route(&agent, change, cx)
                                    });
                                }))
                            }),
                    )
                    .child(div().flex_1())
                    .child(self.delegate_done(busy, true, &p, cx))
            }
            Some(Picking::Orphan(o)) => {
                let agent = o.agent_id.clone();
                let reasoning = routes.as_ref().is_some_and(|r| r.reasoning_supported);
                h_flex()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        kit::danger_btn("delegate-clear", !busy, &p)
                            .when(busy, |el| {
                                el.child(kit::spinner("delegate-clear-spin", 13., p.danger))
                            })
                            .child("Clear saved route")
                            .when(!busy, |el| {
                                el.on_click(cx.listener(move |this, _, window, cx| {
                                    let change = RouteChange {
                                        route: None,
                                        reasoning: reasoning.then_some(None),
                                    };
                                    this.store.update(cx, |s, cx| {
                                        s.set_delegate_route(&agent, change, cx)
                                    });
                                    this.pick_delegate(None, window, cx);
                                }))
                            }),
                    )
                    .child(self.delegate_done(busy, true, &p, cx))
            }
            None => h_flex()
                .justify_end()
                .child(self.delegate_done(busy, false, &p, cx)),
        };

        let card = v_flex()
            .id("delegate-card")
            .w(px(if picking.is_some() { 620. } else { 560. }))
            .max_w(relative(0.94))
            .rounded(px(16.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .px_4()
                    .pt_4()
                    .pb_3()
                    .gap_3()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(title))
                            .child(div().text_xs().text_color(p.dim).child(description)),
                    )
                    .child(
                        kit::icon_button(
                            "delegate-close",
                            if picking.is_some() {
                                IconName::ArrowLeft
                            } else {
                                IconName::X
                            },
                            if picking.is_some() {
                                "Back to delegate routing"
                            } else {
                                "Close"
                            },
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.delegates.picking.is_some() {
                                    this.pick_delegate(None, window, cx);
                                } else {
                                    this.delegates.open = false;
                                    cx.notify();
                                }
                            },
                        )),
                    ),
            )
            .child(div().px_4().child(body))
            .child(div().px_4().py_3().child(footer));
        let scrim = modal_scrim("scrim-delegates", radii, &p);
        Some(self.centered(scrim, div().child(card), cx))
    }

    fn delegate_done(
        &self,
        busy: bool,
        back: bool,
        p: &Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        kit::flat_btn("delegate-done", !busy, true, p)
            .child("Done")
            .when(!busy, |el| {
                el.on_click(cx.listener(move |this, _, window, cx| {
                    if back {
                        this.pick_delegate(None, window, cx);
                    } else {
                        this.delegates.open = false;
                        cx.notify();
                    }
                }))
            })
    }

    /// Notices, the persistence line and one row per delegate.
    fn delegate_list(
        &self,
        owner: &str,
        state: &DelegateState,
        inherited: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let store = self.store.read(cx);
        let models = store.models(owner).to_vec();
        let busy = state.pending.is_some() || state.loading;
        let mut out = v_flex().gap_3();
        if state.conflict {
            out = out.child(
                notice(
                    IconName::TriangleAlert,
                    p.warn,
                    "Routes changed elsewhere. The latest setup is shown; review it before choosing again.",
                    p,
                )
                .child(
                    kit::flat_btn("delegate-conflict-ok", true, true, p)
                        .child("Dismiss")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.dismiss_delegate_conflict(cx))
                        })),
                ),
            );
        }
        if let Some(error) = &state.error {
            out = out.child(
                notice(IconName::CircleAlert, p.danger, error, p).child(
                    kit::flat_btn("delegate-retry", true, true, p)
                        .child("Retry")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.refresh_delegates(cx))
                        })),
                ),
            );
        }
        let Some(routes) = state.routes.clone() else {
            return out.child(empty_state(
                if state.loading {
                    kit::spinner("delegate-loading", 18., p.muted).into_any_element()
                } else {
                    kit::icon(IconName::Waypoints, 18., p.muted).into_any_element()
                },
                if state.loading {
                    "Reading delegate routes"
                } else {
                    "Delegate routing is unavailable"
                },
                if state.loading {
                    "Nothing in the session changes while this loads."
                } else {
                    "This agent does not expose delegate routes for this session."
                },
                p,
            ));
        };

        // Saved with the session (and its revision), or for this run only.
        let persistence = h_flex()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .gap_1p5()
                    .text_xs()
                    .text_color(p.muted)
                    .child(kit::icon(
                        if routes.durable {
                            IconName::HardDrive
                        } else {
                            IconName::CloudOff
                        },
                        13.,
                        p.muted,
                    ))
                    .child(if routes.durable {
                        "Saved with this session"
                    } else {
                        "For this run only"
                    })
                    .children(
                        routes
                            .revision
                            .filter(|_| routes.durable)
                            .map(|r| div().text_color(p.dim).child(format!("revision {r}"))),
                    ),
            )
            .child(
                kit::flat_btn("delegate-refresh", !busy, true, p)
                    .child(if state.loading {
                        kit::spinner("delegate-refresh-spin", 13., p.muted).into_any_element()
                    } else {
                        kit::icon(IconName::RefreshCw, 14., p.text).into_any_element()
                    })
                    .child("Refresh")
                    .when(!busy, |el| {
                        el.on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.refresh_delegates(cx))
                        }))
                    }),
            );
        out = out.child(persistence);
        if !routes.editable {
            out = out.child(notice(
                IconName::Waypoints,
                p.muted,
                "This is a delegated session. Configure routing on its parent session.",
                p,
            ));
        }

        if routes.assignments.is_empty() {
            out = out.child(empty_state(
                kit::icon(IconName::Waypoints, 18., p.muted).into_any_element(),
                "No configurable delegates",
                "This profile has no delegate roles for this session.",
                p,
            ));
        } else {
            let enabled = routes.editable && !busy;
            let (_, hover) = kit::subtle(p);
            let rows: Vec<AnyElement> = routes
                .assignments
                .iter()
                .enumerate()
                .map(|(ix, a)| {
                    let agent = a.agent_id.clone();
                    let pending = state.pending.as_deref() == Some(a.agent_id.as_str());
                    let model = a
                        .route
                        .as_ref()
                        .and_then(|r| models.iter().find(|m| m.key() == r.key()));
                    let unavailable = a.route.is_some() && model.is_none();
                    // The peer's name from the catalog, else the route's node id.
                    let peer = model
                        .and_then(|m| m.node.clone())
                        .or_else(|| a.route.as_ref().and_then(|r| r.node_id.clone()));
                    kit::card_row(ix == 0, p)
                        .id(kit::keyed("delegate-row", a.agent_id.clone()))
                        .when(enabled, |el| {
                            el.cursor_pointer()
                                .hover(move |s| s.bg(hover))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.pick_delegate(Some(agent.clone()), window, cx)
                                }))
                        })
                        .child(kit::identicon(&a.agent_id, 30.))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_semibold()
                                                .truncate()
                                                .child(a.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_family(kit::MONO)
                                                .text_color(p.dim)
                                                .child(a.agent_id.clone()),
                                        ),
                                )
                                .child(div().text_xs().text_color(p.muted).truncate().child(
                                    if a.description.is_empty() {
                                        "Profile delegate".to_string()
                                    } else {
                                        a.description.clone()
                                    },
                                )),
                        )
                        .child(
                            v_flex()
                                .flex_none()
                                .max_w(px(220.))
                                .items_end()
                                .gap_0p5()
                                .text_xs()
                                .child(if pending {
                                    h_flex()
                                        .gap_1p5()
                                        .text_color(p.muted)
                                        .child(kit::spinner(
                                            kit::keyed("delegate-row-spin", a.agent_id.clone()),
                                            12.,
                                            p.muted,
                                        ))
                                        .child("Updating…")
                                } else {
                                    h_flex()
                                        .gap_1p5()
                                        .min_w_0()
                                        // A model served by a mesh peer: an icon
                                        // before its name, the peer on hover.
                                        .children(peer.clone().map(|node| {
                                            div()
                                                .id(kit::keyed("delegate-node", a.agent_id.clone()))
                                                .flex_none()
                                                .child(kit::icon(IconName::Network, 12., p.muted))
                                                .tooltip(move |window, cx| {
                                                    kit::wrap_tip(format!("Runs on {node}"))
                                                        .build(window, cx)
                                                })
                                        }))
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_medium()
                                                .truncate()
                                                .text_color(if unavailable {
                                                    p.warn
                                                } else {
                                                    p.text
                                                })
                                                .child(route_label(a.route.as_ref(), &models)),
                                        )
                                        .when(a.route.is_some(), |el| {
                                            el.child(kit::chip("Override", p))
                                        })
                                })
                                .when(routes.reasoning_supported && !pending, |el| {
                                    el.child(
                                        h_flex()
                                            .gap_1()
                                            .text_color(p.dim)
                                            .child(kit::icon(IconName::Brain, 11., p.dim))
                                            .child(reasoning_text(
                                                a.reasoning.as_deref(),
                                                inherited,
                                            )),
                                    )
                                }),
                        )
                        .into_any_element()
                })
                .collect();
            out = out.child(
                div()
                    .id("delegate-list")
                    .max_h(px(380.))
                    .overflow_y_scroll()
                    // The dialog's surface, not the page, is behind the card.
                    .child(
                        kit::card(p)
                            .children(rows)
                            .child(kit::corner_mask(12., p.surface)),
                    ),
            );
        }

        if !routes.orphans.is_empty() {
            let enabled = routes.editable && !busy;
            out = out.child(
                v_flex()
                    .gap_1p5()
                    .child(div().text_sm().font_semibold().child("Removed delegates"))
                    .child(div().text_xs().text_color(p.muted).child(
                        "These saved routes no longer match the profile. Clear them when they are no longer needed.",
                    ))
                    .children(routes.orphans.iter().map(|o| {
                        let agent = o.agent_id.clone();
                        let pending = state.pending.as_deref() == Some(o.agent_id.as_str());
                        kit::flat_btn(kit::keyed("delegate-orphan", o.agent_id.clone()), enabled, true, p)
                            .justify_start()
                            .child(kit::icon(IconName::TriangleAlert, 13., p.warn))
                            .child(div().font_family(kit::MONO).child(o.agent_id.clone()))
                            .child(div().flex_1().min_w_0().truncate().text_color(p.muted).child(
                                route_summary(
                                    o.route.as_ref(),
                                    o.reasoning.as_deref(),
                                    routes.reasoning_supported,
                                    inherited,
                                    &models,
                                ),
                            ))
                            .child(if pending {
                                kit::spinner(kit::keyed("orphan-spin", o.agent_id.clone()), 13., p.muted)
                                    .into_any_element()
                            } else {
                                kit::icon(IconName::RotateCcw, 13., p.muted).into_any_element()
                            })
                            .when(enabled, |el| {
                                el.on_click(cx.listener(move |this, _, window, cx| {
                                    this.pick_delegate(Some(agent.clone()), window, cx)
                                }))
                            })
                    })),
            );
        }
        out
    }

    /// One delegate: unavailable-route notice, the model rows and reasoning.
    fn delegate_picker(
        &self,
        owner: &str,
        a: &Assignment,
        state: &DelegateState,
        inherited: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let store = self.store.read(cx);
        let routes = state.routes.clone().unwrap_or_default();
        let unavailable = a
            .route
            .as_ref()
            .filter(|r| !store.models(owner).iter().any(|m| m.key() == r.key()))
            .cloned();
        let busy = state.pending.is_some() || !routes.editable;
        let rows = self.model_rows(cx);
        let search = self.model_search(owner, p, cx);
        let reasoning = routes.reasoning_supported.then(|| {
            let current = a.reasoning.clone();
            let inherit_label = match inherited {
                Some(l) => format!("Inherit · {l}"),
                None => "Inherit".into(),
            };
            let choices = std::iter::once((None, inherit_label)).chain(
                REASONING
                    .iter()
                    .map(|(v, l)| (Some(v.to_string()), l.to_string())),
            );
            v_flex()
                .gap_1p5()
                .pt_2()
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .font_semibold()
                        .text_color(p.dim)
                        .child(kit::icon(IconName::Brain, 12., p.dim))
                        .child("Reasoning effort"),
                )
                .child(kit::segment_bar(p).children(choices.enumerate().map(
                    |(ix, (value, label))| {
                        let agent = a.agent_id.clone();
                        let selected = value == current;
                        kit::segment(("delegate-reasoning", ix), selected, p)
                            .flex_1()
                            .label(label)
                            .inactive(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if selected {
                                    return;
                                }
                                let route = this
                                    .store
                                    .read(cx)
                                    .delegate_state()
                                    .and_then(|s| s.routes.as_ref())
                                    .and_then(|r| {
                                        r.assignments.iter().find(|x| x.agent_id == agent)
                                    })
                                    .and_then(|x| x.route.clone());
                                let change = RouteChange {
                                    route,
                                    reasoning: Some(value.clone()),
                                };
                                this.store
                                    .update(cx, |s, cx| s.set_delegate_route(&agent, change, cx));
                            }))
                    },
                )))
                .child(
                    div()
                        .text_xs()
                        .text_color(p.muted)
                        .child("Inherit the parent session, or set this delegate's own effort."),
                )
        });
        v_flex()
            .gap_2()
            .children(unavailable.map(|r| {
                notice(
                    IconName::CloudOff,
                    p.warn,
                    &format!(
                        "The current route {}{} is unavailable. It is kept until you replace or reset it.",
                        r.model_id,
                        r.node_id.as_deref().map(|n| format!(" on {n}")).unwrap_or_default()
                    ),
                    p,
                )
            }))
            .child(search)
            .child(
                v_flex()
                    .id("delegate-model-list")
                    .h(px(LIST_H))
                    .overflow_y_scroll()
                    .track_scroll(&self.model_scroll)
                    .when(busy, |el| el.opacity(0.6))
                    .children(rows)
                    .map(|el| self.smooth_scroll.region("delegate-model-list", el)),
            )
            .children(reasoning)
    }

    /// "Configure · N" of Session options, when the session has delegates.
    pub(super) fn delegate_option_row(&self, p: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let store = self.store.read(cx);
        let view = store.session.as_ref()?;
        if !store.supports_delegate_routing(&view.agent_id) {
            return None;
        }
        let state = store.delegate_state().cloned().unwrap_or_default();
        if !state.has_routes() {
            return None;
        }
        let count = state.routes.as_ref().map_or(0, |r| r.assignments.len());
        Some(
            h_flex()
                .gap_2()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_medium().child("Delegate routing"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child("Model per delegate."),
                        ),
                )
                .child(
                    kit::flat_btn("open-delegates", !state.loading, true, p)
                        .child(if state.loading {
                            kit::spinner("delegates-row-spin", 13., p.muted).into_any_element()
                        } else {
                            kit::icon(IconName::Waypoints, 14., p.text).into_any_element()
                        })
                        .child(if state.error.is_some() {
                            "Retry".to_string()
                        } else if count > 0 {
                            format!("Configure · {count}")
                        } else {
                            "Configure".to_string()
                        })
                        .when(!state.loading, |el| {
                            el.on_click(cx.listener(|this, _, window, cx| {
                                this.open_delegate_dialog(window, cx)
                            }))
                        }),
                ),
        )
    }
}

enum Picking {
    Delegate(Assignment),
    Orphan(Orphan),
}

fn route_label(route: Option<&Route>, models: &[crate::store::Model]) -> String {
    match route {
        None => "Profile default".into(),
        Some(r) => models
            .iter()
            .find(|m| m.key() == r.key())
            .map_or_else(|| r.model_id.clone(), |m| m.label.clone()),
    }
}

fn reasoning_text(reasoning: Option<&str>, inherited: Option<&str>) -> String {
    match reasoning {
        None => inherited.map_or_else(|| "Inherit".into(), |l| format!("Inherit · {l}")),
        Some(r) => REASONING
            .iter()
            .find(|(v, _)| *v == r)
            .map_or_else(|| r.to_string(), |(_, l)| l.to_string()),
    }
}

fn route_summary(
    route: Option<&Route>,
    reasoning: Option<&str>,
    reasoning_supported: bool,
    inherited: Option<&str>,
    models: &[crate::store::Model],
) -> String {
    let label = route_label(route, models);
    if reasoning_supported {
        format!("{label} · {}", reasoning_text(reasoning, inherited))
    } else {
        label
    }
}

fn notice(icon: IconName, tone: Hsla, text: &str, p: &Palette) -> Div {
    h_flex()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(px(10.))
        .bg(tone.opacity(0.1))
        .text_sm()
        .child(kit::icon(icon, 15., tone))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(p.text)
                .child(SharedString::from(text.to_owned())),
        )
}

fn empty_state(icon: AnyElement, title: &'static str, body: &'static str, p: &Palette) -> Div {
    v_flex()
        .items_center()
        .gap_1p5()
        .py_8()
        .child(icon)
        .child(div().text_sm().font_semibold().child(title))
        .child(div().text_xs().text_color(p.muted).child(body))
}

#[cfg(test)]
mod tests {
    use super::super::{Page, QmtApp};

    #[gpui_kit::test]
    fn dialog_lists_delegates_and_opens_one(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| {
                let mut store = crate::store::automations::schedule_ui_fixture();
                store.session = Some(crate::session::SessionView::new(
                    "test-agent",
                    "s",
                    "/w",
                    "Session",
                ));
                store.set_delegate_routes_for_test(
                    &serde_json::from_str(
                        r#"{"session_id":"s","profile_id":"dev","revision":2,"durable":true,
                            "editable":true,"reasoning_effort_supported":true,
                            "assignments":[{"agent_id":"coder","name":"Coder","description":"",
                              "model":{"model_id":"gone/m"},"configured_default_model_id":null,
                              "reasoning_effort":"high"}],
                            "orphaned_overrides":[{"agent_id":"old","model":null,"reasoning_effort":null}]}"#,
                    )
                    .unwrap(),
                );
                store
            });
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.delegates.open = true;
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                app.pick_delegate(Some("coder".into()), window, cx)
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                // The open delegate's route is the picker's selection.
                assert_eq!(
                    app.delegate_selected_key(cx),
                    Some(Some("gone/m".to_string()))
                );
                // Escape goes back to the list, then closes.
                assert!(app.close_delegate_dialog(cx));
                assert!(app.delegates.open && app.delegates.picking.is_none());
                app.pick_delegate(Some("old".into()), window, cx);
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                assert!(app.close_delegate_dialog(cx) && app.close_delegate_dialog(cx));
                assert!(!app.delegates.open);
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn enter_in_delegate_dialog_routes_the_delegate(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::launch::models_fixture(0);
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        store.session = Some(crate::session::SessionView::new(
            "test-agent",
            "s",
            "/w",
            "Session",
        ));
        store.set_delegate_routes_for_test(
            &serde_json::from_str(
                r#"{"session_id":"s","profile_id":"dev","revision":2,"durable":true,
                    "editable":true,"reasoning_effort_supported":true,
                    "assignments":[{"agent_id":"coder","name":"Coder","description":"",
                      "model":{"model_id":"openai/o3"},"configured_default_model_id":null,
                      "reasoning_effort":null}],
                    "orphaned_overrides":[]}"#,
            )
            .unwrap(),
        );
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.delegates.open = true;
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                app.pick_delegate(Some("coder".into()), window, cx)
            });
            window.render_frame(cx);
        })
        .unwrap();
        // The highlight starts on the delegate's current route (`o3`); one
        // step down is the next model, in the Google group.
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        vcx.simulate_keystrokes("down enter");
        let request: serde_json::Value =
            serde_json::from_str(&requests.try_recv().unwrap()).unwrap();
        assert_eq!(request["method"], "_querymt/session/setDelegateModel");
        assert_eq!(
            request["params"]["agentId"]
                .as_str()
                .or(request["params"]["agent_id"].as_str()),
            Some("coder")
        );
        assert!(request.to_string().contains("google/gemini-2.5-pro"));
        // The session's own model request was not sent.
        assert!(requests.try_recv().is_err());
    }
}
