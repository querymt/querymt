//! Agents page: configured agents, connect / disconnect, add / edit / delete,
//! and a details sheet (runtime, control API, capabilities, log).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Icon, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::kit::ButtonExt as _;
use super::{AgentDialog, Page, QmtApp, kit};
use crate::store::{AgentConfig, ConnState, Endpoint, normalize_address};
use crate::theme::{Palette, palette};

const CAN_SPAWN: bool = cfg!(not(target_family = "wasm"));

/// Full-window backdrop of the app's modals. It rounds itself: GPUI's
/// overflow mask is rectangular, so the frame cannot clip its corners.
pub(super) fn modal_scrim(id: &'static str, radii: Corners<Pixels>, p: &Palette) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .rounded_tl(radii.top_left)
        .rounded_tr(radii.top_right)
        .rounded_bl(radii.bottom_left)
        .rounded_br(radii.bottom_right)
        .bg(gpui_kit::black().opacity(if p.dark { 0.55 } else { 0.28 }))
        .occlude()
}
/// Width of the stats columns of an agent row (four 60px columns).
const STATS_W: f32 = 252.;

impl QmtApp {
    // ---- dialog state --------------------------------------------------------

    pub(super) fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.d_name, &self.d_command, &self.d_address] {
            input.update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        }
        self.d_command
            .update(cx, |s, cx| s.set_value("qmtcode --acp", window, cx));
        self.dialog = Some(AgentDialog {
            editing: None,
            websocket: !CAN_SPAWN,
            error: None,
        });
        self.d_name.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn open_edit_dialog(
        &mut self,
        config: &AgentConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (command, address) = match &config.endpoint {
            Endpoint::Process { command } => (command.clone(), String::new()),
            Endpoint::WebSocket { .. } => (String::new(), config.endpoint_label()),
        };
        self.d_name
            .update(cx, |s, cx| s.set_value(config.name.clone(), window, cx));
        self.d_command
            .update(cx, |s, cx| s.set_value(command, window, cx));
        self.d_address
            .update(cx, |s, cx| s.set_value(address, window, cx));
        self.details = None;
        self.dialog = Some(AgentDialog {
            editing: Some(config.id.clone()),
            websocket: config.is_websocket(),
            error: None,
        });
        cx.notify();
    }

    pub(super) fn save_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let name = self.d_name.read(cx).value().trim().to_string();
        let endpoint = if dialog.websocket {
            match normalize_address(&self.d_address.read(cx).value()) {
                Ok((address, secure)) => Endpoint::WebSocket { address, secure },
                Err(err) => {
                    dialog.error = Some(err);
                    cx.notify();
                    return;
                }
            }
        } else {
            let command = self.d_command.read(cx).value().trim().to_string();
            if command.is_empty() {
                dialog.error = Some("Enter the command that starts the ACP agent.".into());
                cx.notify();
                return;
            }
            Endpoint::Process { command }
        };
        let name = if name.is_empty() {
            match &endpoint {
                Endpoint::WebSocket { address, .. } => address.clone(),
                Endpoint::Process { command } => command
                    .split_whitespace()
                    .next()
                    .unwrap_or("agent")
                    .to_string(),
            }
        } else {
            name
        };
        let editing = dialog.editing.clone();
        self.dialog = None;
        self.store.update(cx, |s, cx| match editing {
            Some(id) => s.update_config(&id, name, endpoint, cx),
            None => {
                s.add_config(name, endpoint, cx);
            }
        });
        cx.notify();
    }

    // ---- page ---------------------------------------------------------------

    pub(super) fn render_agents(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let store = self.store.read(cx);
        let configs = store.configs.clone();
        let connected = store.connected_ids().len();
        let refreshing = store.configs.iter().any(|c| {
            store.state(&c.id) == ConnState::Connecting
                || store.runtime(&c.id).is_some_and(|r| r.busy)
        });
        let attention: Vec<(AgentConfig, String)> = configs
            .iter()
            .filter_map(|c| store.needs_attention(&c.id).map(|r| (c.clone(), r)))
            .collect();

        let header = kit::page_header(
            "Agents",
            (!configs.is_empty()).then(|| {
                kit::count_pill(format!("{connected}/{}", configs.len()), "connected", &p)
                    .into_any_element()
            }),
            Some("Configure connections, inspect status, and control agent runtimes."),
            vec![
                kit::flat_btn("agents-add", true, true, &p)
                    .child(kit::icon(IconName::Plus, 14., p.text))
                    .child("Add agent")
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx)))
                    .into_any_element(),
                kit::loading_icon_button(
                    "agents-refresh",
                    IconName::RefreshCw,
                    "Refresh agents",
                    refreshing,
                    24.,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.store.update(cx, |s, cx| s.refresh_all(cx))),
                )
                .into_any_element(),
            ],
            &p,
        );

        let list = if configs.is_empty() {
            kit::card(&p)
                .items_center()
                .gap_2()
                .p_8()
                .child(kit::icon_tile(IconName::Bot, false, &p))
                .child(div().font_semibold().child("No agents configured"))
                .child(div().text_sm().text_color(p.muted).child(if CAN_SPAWN {
                    "Add a local ACP command, or connect to an ACP WebSocket server on another machine."
                } else {
                    "Connect to an ACP WebSocket server (host:port)."
                }))
                .child(
                    kit::accent_btn("empty-add", true, true, &p)
                        .child(kit::icon(IconName::Plus, 14., p.on_accent))
                        .child("Add agent")
                        .on_click(cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx))),
                )
                .into_any_element()
        } else {
            let mut rows = Vec::new();
            for (ix, c) in configs.iter().enumerate() {
                rows.push(self.agent_row(c, ix == 0, &p, cx).into_any_element());
            }
            kit::card(&p)
                .children(rows)
                .child(kit::card_mask(&p))
                .into_any_element()
        };

        let attention_section = (!attention.is_empty()).then(|| {
            let count = attention.len();
            v_flex()
                .gap_3()
                .child(kit::section_header(
                    "Needs attention",
                    "Agents with connection or runtime issues.",
                    Some(kit::chip(count.to_string(), &p).into_any_element()),
                    &p,
                ))
                .child(
                    kit::card(&p).children(attention.into_iter().enumerate().map(
                        |(ix, (c, reason))| {
                            let (id, details) = (c.id.clone(), c.id.clone());
                            kit::card_row(ix == 0, &p)
                                .child(kit::icon_tile(IconName::TriangleAlert, false, &p))
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div().text_sm().font_semibold().child(c.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .mt(px(3.2))
                                                .text_xs()
                                                .text_color(p.warn)
                                                .truncate()
                                                .child(reason),
                                        ),
                                )
                                .child(
                                    kit::flat_btn(
                                        kit::keyed("attention-details", c.id.clone()),
                                        true,
                                        true,
                                        &p,
                                    )
                                    .child("Details")
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.details = Some(details.clone());
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    kit::flat_btn(
                                        kit::keyed("attention-retry", c.id.clone()),
                                        true,
                                        true,
                                        &p,
                                    )
                                    .child(kit::icon(IconName::RotateCcw, 14., p.text))
                                    .child(if c.is_websocket() {
                                        "Reconnect"
                                    } else {
                                        "Restart"
                                    })
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.store.update(cx, |s, cx| s.connect(&id, cx))
                                        },
                                    )),
                                )
                        },
                    )),
                )
        });

        kit::page(
            [Some(header.into_any_element()), Some(list)]
                .into_iter()
                .flatten()
                .chain(attention_section.map(IntoElement::into_any_element)),
        )
    }

    fn agent_row(
        &self,
        c: &AgentConfig,
        first: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.read(cx);
        let state = store.state(&c.id);
        let rt = store.runtime(&c.id);
        let sessions = rt.map_or(0, |r| {
            r.sessions.iter().filter(|s| s.root && s.open).count()
        });
        let running = rt.map_or(0, |r| {
            r.sessions.iter().filter(|s| s.root && s.running()).count()
        });
        let models = rt.map_or(0, |r| r.models.len());
        let version = rt.and_then(|r| r.agent_version.clone());
        let host = rt.and_then(|r| r.mesh.as_ref()?.hostname.clone());
        let peers = rt.and_then(|r| r.mesh.as_ref().map(|m| m.peers));
        let is_primary = store.primary_agent().as_deref() == Some(c.id.as_str());
        let issue = store.needs_attention(&c.id);
        let live = matches!(state, ConnState::Connected | ConnState::Connecting);
        let connected = state == ConnState::Connected;
        let status = kit::status_color(&state, p);
        let websocket = c.is_websocket();
        let (_, subtle_surface) = kit::subtle(p);

        // Object icon with the connection state in its corner.
        let avatar = kit::icon_tile(
            if websocket {
                IconName::Globe
            } else {
                IconName::Terminal
            },
            connected,
            p,
        )
        .child(
            div()
                .absolute()
                .right(px(-3.))
                .bottom(px(-3.))
                .size(px(12.))
                .rounded_full()
                .bg(p.card)
                .flex()
                .items_center()
                .justify_center()
                .child(kit::dot(status)),
        );

        let title = h_flex()
            .gap_2()
            .min_w_0()
            .child(
                div()
                    .text_size(px(13.4))
                    .font_semibold()
                    .truncate()
                    .child(c.name.clone()),
            )
            .child(kit::tone_chip(kit::state_label(&state), status))
            .when(is_primary && connected, |el| {
                el.child(kit::tone_chip("default", p.accent))
            })
            .when(c.host_managed, |el| el.child(kit::chip("host managed", p)))
            .when(issue.is_some(), |el| {
                el.child(kit::icon_tone_chip(
                    IconName::TriangleAlert,
                    "Issue",
                    p.warn,
                ))
            });

        let meta = h_flex()
            .mt(px(3.2))
            .gap_1p5()
            .min_w_0()
            .text_xs()
            .text_color(p.muted)
            .child(
                div()
                    .font_family(kit::MONO)
                    .truncate()
                    .child(c.endpoint_label()),
            )
            .when_some(host, |el, h| el.child("·").child(format!("on {h}")))
            .when_some(version, |el, v| {
                el.child("·")
                    .child(div().font_family(kit::MONO).child(format!("v{v}")))
            });

        let stat = |value: String, label: &'static str, color: Hsla| {
            v_flex()
                .w(px(60.))
                .items_end()
                .child(
                    div()
                        .font_family(kit::MONO)
                        .text_sm()
                        .font_semibold()
                        .text_color(color)
                        .child(value),
                )
                .child(div().text_xs().text_color(p.muted).child(label))
        };
        // Fixed width whether or not the agent is connected, so the action
        // icons of every row line up.
        let stats = h_flex()
            .flex_none()
            .w(px(STATS_W))
            .justify_end()
            .gap_1()
            .when(connected, |el| {
                el.child(stat(sessions.to_string(), "open", p.text))
                    .child(stat(
                        running.to_string(),
                        "running",
                        if running > 0 { p.success } else { p.text },
                    ))
                    .child(stat(models.to_string(), "models", p.text))
                    .when_some(peers, |el, n| {
                        el.child(stat(n.to_string(), "peers", p.text))
                    })
            });

        // Visible: start/stop and auto-start. Everything else: the menu.
        let toggle_id = c.id.clone();
        let start_stop = kit::flat_icon_btn(
            kit::keyed("toggle", c.id.clone()),
            if live {
                IconName::Square
            } else {
                IconName::Play
            },
            match (live, websocket) {
                (true, true) => "Disconnect",
                (true, false) => "Stop",
                (false, true) => "Connect",
                (false, false) => "Start",
            },
            p,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.store.update(cx, |s, cx| {
                if live {
                    s.disconnect(&toggle_id, cx)
                } else {
                    s.connect(&toggle_id, cx)
                }
            })
        }));

        // Auto-start state at a glance; it is changed from the menu.
        let auto_start = (!c.host_managed).then(|| {
            let on = c.auto_start;
            let tip = match (on, websocket) {
                (true, true) => "Auto-start on: connects when the app starts",
                (true, false) => "Auto-start on: starts when the app starts",
                (false, _) => "Auto-start off",
            };
            div()
                .id(kit::keyed("auto-state", c.id.clone()))
                .flex_none()
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .child(kit::icon(
                    if on {
                        IconName::Power
                    } else {
                        IconName::PowerOff
                    },
                    15.,
                    if on { p.success } else { p.dim },
                ))
                .tooltip(move |window, cx| kit::wrap_tip(tip).build(window, cx))
        });

        let menu = {
            let app = cx.entity().downgrade();
            let store_h = self.store.clone();
            let (id, config) = (c.id.clone(), c.clone());
            let host_managed = c.host_managed;
            let auto_on = c.auto_start;
            kit::icon_button(
                kit::keyed("agent-menu", c.id.clone()),
                IconName::Ellipsis,
                "Agent actions",
            )
            // Same square as the start/stop button beside it.
            .size(px(32.))
            .px_0()
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), store_h.clone());
                let (i1, i3, i4) = (id.clone(), id.clone(), id.clone());
                let (a5, i5) = (store_h.clone(), id.clone());
                let config = config.clone();
                menu.item(
                    PopupMenuItem::new("Details")
                        .icon(Icon::new(IconName::Info))
                        .on_click(move |_, _, cx| {
                            let _ = a1.update(cx, |this, cx| {
                                this.details = Some(i1.clone());
                                cx.notify();
                            });
                        }),
                )
                .when(live, |m| {
                    m.item(
                        PopupMenuItem::new(if websocket { "Reconnect" } else { "Restart" })
                            .icon(Icon::new(IconName::RotateCcw))
                            .on_click(move |_, _, cx| a4.update(cx, |s, cx| s.connect(&i4, cx))),
                    )
                })
                .when(!host_managed, |m| {
                    m.item(
                        PopupMenuItem::new(if auto_on {
                            "Disable auto-start"
                        } else {
                            "Enable auto-start"
                        })
                        .icon(Icon::new(if auto_on {
                            IconName::PowerOff
                        } else {
                            IconName::Power
                        }))
                        .on_click(move |_, _, cx| {
                            a5.update(cx, |s, cx| s.toggle_auto_start(&i5, cx))
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Edit…")
                            .icon(Icon::new(IconName::Pencil))
                            .on_click(move |_, window, cx| {
                                let _ = a2.update(cx, |this, cx| {
                                    this.open_edit_dialog(&config, window, cx)
                                });
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("Delete…")
                            .icon(Icon::new(IconName::Trash))
                            .on_click(move |_, _, cx| {
                                let _ = a3.update(cx, |this, cx| {
                                    this.confirm_delete = Some(i3.clone());
                                    cx.notify();
                                });
                            }),
                    )
                })
            })
        };

        let name_id = c.id.clone();
        let hover = subtle_surface;
        kit::card_row(first, p)
            .id(kit::keyed("agent-row", c.id.clone()))
            .cursor_pointer()
            .hover(move |el| el.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.details = Some(name_id.clone());
                cx.notify();
            }))
            .child(avatar)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(title)
                    .child(meta)
                    .when_some(
                        match &state {
                            ConnState::Failed(reason) => Some(reason.clone()),
                            _ => None,
                        },
                        |el, reason| {
                            el.child(
                                div()
                                    .mt(px(3.2))
                                    .text_xs()
                                    .text_color(p.warn)
                                    .truncate()
                                    .child(reason),
                            )
                        },
                    ),
            )
            .child(stats)
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .pl_2()
                    // Slot kept for host-managed agents too, for alignment.
                    .child(auto_start.map_or_else(
                        || div().flex_none().size(px(24.)).into_any_element(),
                        IntoElement::into_any_element,
                    ))
                    .child(start_stop)
                    .child(
                        div()
                            .id(kit::keyed("agent-menu-wrap", c.id.clone()))
                            // A little apart from start/stop.
                            .ml_1()
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(menu),
                    ),
            )
    }

    // ---- overlays -----------------------------------------------------------

    pub(super) fn render_overlay(
        &mut self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = *palette(cx);
        let scrim = |id: &'static str| modal_scrim(id, radii, &p);

        if self.confirm_reset {
            let card = self
                .modal_card(&p)
                .w(px(460.))
                .child(div().text_lg().font_semibold().child("Reset app data?"))
                .child(div().text_sm().text_color(p.muted).child(
                    "Configured agents and preferences are removed and the app starts over \
                     like a fresh install. This cannot be undone.",
                ))
                .child(
                    v_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(p.muted)
                        .child("Removed: configured agents, recent models and workspaces, appearance and chat preferences.")
                        .child("Kept: sessions on the agents, provider sign-ins and API keys, project files."),
                )
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(kit::button("reset-cancel").label("Cancel").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.confirm_reset = false;
                                cx.notify();
                            }),
                        ))
                        .child(kit::button("reset-ok").danger().label("Reset").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.confirm_reset = false;
                                this.general_danger_open = false;
                                this.page = Page::Start;
                                this.store.update(cx, |s, cx| s.reset_app_data(cx));
                                this.apply_theme(window, cx);
                            }),
                        )),
                );
            return Some(self.centered(scrim("scrim-reset"), card, cx));
        }

        if let Some(message_id) = self.confirm_undo.clone() {
            let affected = self
                .store
                .read(cx)
                .session
                .as_ref()
                .and_then(|v| v.undo_affected_turns(&message_id))
                .unwrap_or(1);
            let body = if affected > 1 {
                format!(
                    "This rolls back this turn and {} later turn{}. You can use Redo afterward.",
                    affected - 1,
                    if affected == 2 { "" } else { "s" }
                )
            } else {
                "This rolls back the latest turn. You can use Redo afterward.".to_string()
            };
            let card =
                self.modal_card(&p)
                    .w(px(460.))
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child("Undo workspace changes?"),
                    )
                    .child(div().text_sm().text_color(p.muted).child(body))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(kit::button("undo-cancel").label("Cancel").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.confirm_undo = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                kit::button("undo-ok")
                                    .danger()
                                    .label("Undo changes")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(message_id) = this.confirm_undo.take() {
                                            this.store.update(cx, |s, cx| {
                                                s.undo_workspace_to(message_id, cx)
                                            });
                                        }
                                        cx.notify();
                                    })),
                            ),
                    );
            return Some(self.centered(scrim("scrim-undo"), card, cx));
        }

        if let Some(s) = self.confirm_session_delete.clone() {
            let agent = self
                .store
                .read(cx)
                .config(&s.agent_id)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "the agent".into());
            let card = self
                .modal_card(&p)
                .w(px(440.))
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .child(format!("Delete {}?", s.display_title())),
                )
                .child(div().text_sm().text_color(p.muted).child(format!(
                    "The session and its history will be permanently removed from {agent}. This cannot be undone."
                )))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            kit::button("session-del-cancel")
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_session_delete = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            kit::button("session-del-ok")
                                .danger()
                                .label("Delete")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.confirm_session_delete = None;
                                    if this.store.read(cx).session.as_ref().is_some_and(|v| {
                                        v.agent_id == s.agent_id && v.session_id == s.id
                                    }) && this.page == Page::Session
                                    {
                                        this.page = Page::Sessions;
                                    }
                                    this.store.update(cx, |st, cx| {
                                        st.delete_session(&s.agent_id, &s.id, cx)
                                    });
                                })),
                        ),
                );
            return Some(self.centered(scrim("scrim-session-delete"), card, cx));
        }

        if let Some(id) = self.confirm_delete.clone() {
            let name = self
                .store
                .read(cx)
                .config(&id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let card = self
                .modal_card(&p)
                .w(px(420.))
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .child(format!("Delete {name}?")),
                )
                .child(div().text_sm().text_color(p.muted).child(
                    "The configuration is removed from this app. Sessions stay on the agent.",
                ))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            kit::button("del-cancel")
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_delete = None;
                                    cx.notify();
                                })),
                        )
                        .child(kit::button("del-ok").danger().label("Delete").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.confirm_delete = None;
                                if this.details.as_deref() == Some(id.as_str()) {
                                    this.details = None;
                                }
                                this.store.update(cx, |s, cx| s.delete_config(&id, cx));
                            }),
                        )),
                );
            return Some(self.centered(scrim("scrim-delete"), card, cx));
        }

        // New mesh invite: a dialog like Add agent, not an inline form.
        if self.invite_form
            && let Some(agent) = self.store.read(cx).primary_agent()
        {
            let creating = self.store.read(cx).is_pending(&agent, "invite");
            let ttl = self.invite_ttl;
            let field = |label: &'static str, help: &'static str, input: AnyElement| {
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_medium().child(label))
                    .child(input)
                    .child(div().text_xs().text_color(p.dim).child(help))
            };
            let card = self
                .modal_card(&p)
                .w(px(460.))
                .child(
                    v_flex()
                        .child(div().text_lg().font_semibold().child("New invite"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.muted)
                                .child("A link another machine uses to join this mesh."),
                        ),
                )
                .child(field(
                    "Expires after",
                    "The link stops working after this time.",
                    kit::segment_bar(&p)
                        .children(super::mesh::INVITE_TTLS.iter().enumerate().map(
                            |(ix, (v, label))| {
                                let v: &'static str = v;
                                kit::segment(("ttl", ix), ttl == v, &p)
                                    .flex_1()
                                    .label(*label)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.invite_ttl = v;
                                        cx.notify();
                                    }))
                            },
                        ))
                        .into_any_element(),
                ))
                .child(field(
                    "Maximum uses",
                    "How many machines can join with it. Leave empty for unlimited.",
                    Input::new(&self.invite_uses).into_any_element(),
                ))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            kit::button("invite-cancel")
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.invite_form = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            kit::button("invite-create")
                                .primary()
                                .label("Create invite")
                                .map(|b| kit::loading_button(b, "invite-spinner", creating, 12.))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.create_invite(&agent, cx)
                                })),
                        ),
                );
            return Some(self.centered(scrim("scrim-invite"), card, cx));
        }

        if let Some(dialog) = &self.dialog {
            let websocket = dialog.websocket;
            let error = dialog.error.clone();
            let editing = dialog.editing.is_some();
            let field = |label: &'static str, help: &'static str, input: AnyElement| {
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_medium().child(label))
                    .child(input)
                    .child(div().text_xs().text_color(p.dim).child(help))
            };
            let card = self.modal_card(&p)
                .w(px(520.))
                .child(
                    v_flex()
                        .child(div().text_lg().font_semibold().child(if editing { "Edit agent" } else { "Add agent" }))
                        .child(div().text_sm().text_color(p.muted).child(
                            "Run a local ACP process, or connect to an already-running qmtcode WebSocket server.",
                        )),
                )
                .child(field("Agent name", "Shown in the sidebar and session controls.", Input::new(&self.d_name).into_any_element()))
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().font_medium().child("Connection type"))
                        .child(
                            kit::segment_bar(&p)
                                .child(
                                    kit::segment("t-process", !websocket, &p)
                                        .flex_1()
                                        .icon(IconName::Terminal)
                                        .label("Local process")
                                        .inactive(!CAN_SPAWN)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(d) = &mut this.dialog {
                                                d.websocket = false;
                                                d.error = None;
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    kit::segment("t-ws", websocket, &p)
                                        .flex_1()
                                        .icon(IconName::Globe)
                                        .label("WebSocket server")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(d) = &mut this.dialog {
                                                d.websocket = true;
                                                d.error = None;
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(div().text_xs().text_color(p.dim).child(if CAN_SPAWN {
                            "Local processes are managed by this app. WebSocket agents run independently, locally or remotely."
                        } else {
                            "The browser can only connect to WebSocket agents."
                        })),
                )
                .child(if websocket {
                    field(
                        "Server address",
                        "host:port of `qmtcode --acp-ws`. Use wss://host:port for TLS.",
                        Input::new(&self.d_address).into_any_element(),
                    )
                } else {
                    field(
                        "Command line",
                        "Executable and arguments that start the ACP agent over stdio.",
                        Input::new(&self.d_command).into_any_element(),
                    )
                })
                .when_some(error, |el, err| {
                    el.child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .text_color(p.danger)
                            .child(kit::icon(IconName::CircleAlert, 14., p.danger))
                            .child(err),
                    )
                })
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(kit::button("dlg-cancel").label("Cancel").on_click(cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            cx.notify();
                        })))
                        .child(
                            kit::button("dlg-save")
                                .primary()
                                .label(if editing { "Save changes" } else { "Add agent" })
                                .on_click(cx.listener(|this, _, _, cx| this.save_dialog(cx))),
                        ),
                );
            return Some(self.centered(scrim("scrim-dialog"), card, cx));
        }

        let id = self.details.clone()?;
        Some(self.render_details(&id, scrim("scrim-details"), &p, cx))
    }

    pub(super) fn modal_card(&self, p: &Palette) -> Div {
        v_flex()
            .gap_4()
            .p_5()
            .rounded(px(16.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
    }

    pub(super) fn centered(
        &self,
        scrim: Stateful<Div>,
        card: Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        scrim
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.close_topmost() {
                    this.close_automation_overlay(cx);
                }
                cx.notify();
            }))
            .child(
                card.id("modal-card")
                    .on_click(|_, _, cx| cx.stop_propagation()),
            )
            .into_any_element()
    }

    fn render_details(
        &self,
        id: &str,
        scrim: Stateful<Div>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let Some(c) = store.config(id).cloned() else {
            return div().into_any_element();
        };
        let state = store.state(id);
        let rt = store.runtime(id);
        let tone = |ok: bool| if ok { p.success } else { p.status_muted };

        let sessions = rt.map_or(0, |r| r.sessions.len());
        let open = rt.map_or(0, |r| {
            r.sessions.iter().filter(|s| s.root && s.open).count()
        });
        let control = rt.and_then(|r| r.control_version());
        let transports = rt
            .map(|r| r.capability_flags("transport"))
            .unwrap_or_default();
        let features = rt
            .map(|r| r.capability_flags("features"))
            .unwrap_or_default();
        let methods = rt.map_or(0, |r| r.method_count());
        let logs: Vec<String> = rt
            .map(|r| r.logs.iter().rev().take(80).cloned().collect())
            .unwrap_or_default();
        let mesh = rt.and_then(|r| r.mesh.clone());

        let row = |label: &'static str, value: String, mono: bool| {
            h_flex()
                .gap_4()
                .py_1p5()
                .child(
                    div()
                        .w(px(120.))
                        .flex_none()
                        .text_sm()
                        .text_color(p.dim)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .truncate()
                        .when(mono, |el| el.font_family(kit::MONO))
                        .child(value),
                )
        };
        let section = |title: &'static str, desc: &'static str| {
            v_flex()
                .pt_2()
                .child(div().font_semibold().child(title))
                .child(div().text_xs().text_color(p.dim).child(desc))
        };
        let chips = |items: Vec<String>| {
            h_flex()
                .flex_wrap()
                .gap_1()
                .children(items.into_iter().map(|t| kit::chip(t, p)))
        };

        let refresh_id = c.id.clone();
        let panel = v_flex()
            .id("details-panel")
            // Centred like the other modals; tall content scrolls in the body.
            .w(px(520.))
            .max_w(relative(0.92))
            .h(relative(0.86))
            .max_h(px(760.))
            .rounded(px(16.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .gap_3()
                    .p_4()
                    .border_b_1()
                    .border_color(p.border)
                    .child(kit::dot(kit::status_color(&state, p)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_lg()
                                    .font_semibold()
                                    .truncate()
                                    .child(c.name.clone()),
                            )
                            .child(div().text_xs().text_color(p.dim).child(c.transport_label())),
                    )
                    .child(
                        kit::icon_button(
                            "details-refresh",
                            IconName::RefreshCw,
                            "Refresh capabilities",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.refresh(&refresh_id, cx))
                        })),
                    )
                    .child(
                        kit::icon_button("details-close", IconName::X, "Close").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.details = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .id("details-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(kit::stat_card(
                                "Connection",
                                kit::state_label(&state),
                                kit::status_color(&state, p),
                                p,
                            ))
                            .child(kit::stat_card(
                                "Sessions",
                                format!("{open} / {sessions}"),
                                p.text,
                                p,
                            ))
                            .child(kit::stat_card(
                                "Control API",
                                control.map_or("—".into(), |v| format!("v{v}")),
                                tone(control.is_some()),
                                p,
                            )),
                    )
                    .when_some(
                        match &state {
                            ConnState::Failed(r) => Some(r.clone()),
                            _ => None,
                        },
                        |el, reason| {
                            el.child(
                                h_flex()
                                    .gap_2()
                                    .p_3()
                                    .rounded(px(10.))
                                    .bg(p.warn.opacity(0.1))
                                    .text_sm()
                                    .child(kit::icon(IconName::TriangleAlert, 15., p.warn))
                                    .child(div().flex_1().min_w_0().child(reason)),
                            )
                        },
                    )
                    .child(section("Runtime", "How this app reaches the agent."))
                    .child(
                        v_flex()
                            .child(row("Endpoint", c.endpoint_label(), true))
                            .when(c.is_websocket(), |el| {
                                el.child(row(
                                    "ACP URL",
                                    match &c.endpoint {
                                        Endpoint::WebSocket { address, secure } => {
                                            format!(
                                                "{}://{address}/acp/ws",
                                                if *secure { "wss" } else { "ws" }
                                            )
                                        }
                                        _ => String::new(),
                                    },
                                    true,
                                ))
                            })
                            .when_some(rt.and_then(|r| r.pid), |el, pid| {
                                el.child(row("Process ID", pid.to_string(), true))
                            })
                            .child(row(
                                "Agent",
                                rt.and_then(|r| r.agent_title.clone()).unwrap_or("—".into()),
                                false,
                            ))
                            .child(row(
                                "Version",
                                rt.and_then(|r| r.agent_version.clone())
                                    .unwrap_or("—".into()),
                                true,
                            ))
                            .child(row(
                                "Auto-start",
                                if c.auto_start {
                                    "On".into()
                                } else {
                                    "Off".into()
                                },
                                false,
                            ))
                            .when_some(mesh, |el, m| {
                                el.child(row("Mesh host", m.hostname.unwrap_or("—".into()), false))
                                    .child(row(
                                        "Mesh peers",
                                        format!(
                                            "{} via {}",
                                            m.peers,
                                            m.transport.unwrap_or("?".into())
                                        ),
                                        false,
                                    ))
                            }),
                    )
                    .child(section(
                        "Capabilities",
                        "Transports and product features reported by the agent.",
                    ))
                    .child(if control.is_some() {
                        v_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .gap_3()
                                    .items_start()
                                    .child(
                                        div()
                                            .w(px(120.))
                                            .flex_none()
                                            .text_sm()
                                            .text_color(p.dim)
                                            .child("Transport"),
                                    )
                                    .child(chips(transports)),
                            )
                            .child(
                                h_flex()
                                    .gap_3()
                                    .items_start()
                                    .child(
                                        div()
                                            .w(px(120.))
                                            .flex_none()
                                            .text_sm()
                                            .text_color(p.dim)
                                            .child("Features"),
                                    )
                                    .child(chips(features)),
                            )
                            .child(row("Methods", methods.to_string(), false))
                            .into_any_element()
                    } else {
                        div()
                            .text_sm()
                            .text_color(p.dim)
                            .child("Not loaded. Connect the agent to read its capabilities.")
                            .into_any_element()
                    })
                    .child(section("Log", "Most recent first."))
                    .child(
                        v_flex()
                            .p_3()
                            .gap_0p5()
                            .rounded(px(10.))
                            .bg(p.bg)
                            .border_1()
                            .border_color(p.border)
                            .font_family(kit::MONO)
                            .text_xs()
                            .text_color(p.muted)
                            .when(logs.is_empty(), |el| el.child("No log lines yet."))
                            .children(logs.into_iter().map(|l| div().child(l))),
                    )
                    .map(|el| self.smooth_scroll.region("agent-details", el)),
            );

        scrim
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, _, cx| {
                this.details = None;
                cx.notify();
            }))
            .child(panel)
            .into_any_element()
    }
}
