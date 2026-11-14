//! Agents page: configured agents, connect / disconnect, add / edit / delete,
//! and a details sheet (runtime, control API, capabilities, log).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{AgentDialog, QmtApp, kit};
use super::kit::ButtonExt as _;
use crate::store::{AgentConfig, ConnState, Endpoint, normalize_address};
use crate::theme::{Palette, palette};

const CAN_SPAWN: bool = cfg!(not(target_family = "wasm"));

impl QmtApp {
    // ---- dialog state --------------------------------------------------------

    pub(super) fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.d_name, &self.d_command, &self.d_address] {
            input.update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        }
        self.d_command.update(cx, |s, cx| s.set_value("qmtcode --acp", window, cx));
        self.dialog = Some(AgentDialog { editing: None, websocket: !CAN_SPAWN, error: None });
        self.d_name.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn open_edit_dialog(&mut self, config: &AgentConfig, window: &mut Window, cx: &mut Context<Self>) {
        let (command, address) = match &config.endpoint {
            Endpoint::Process { command } => (command.clone(), String::new()),
            Endpoint::WebSocket { .. } => (String::new(), config.endpoint_label()),
        };
        self.d_name.update(cx, |s, cx| s.set_value(config.name.clone(), window, cx));
        self.d_command.update(cx, |s, cx| s.set_value(command, window, cx));
        self.d_address.update(cx, |s, cx| s.set_value(address, window, cx));
        self.details = None;
        self.dialog = Some(AgentDialog { editing: Some(config.id.clone()), websocket: config.is_websocket(), error: None });
        cx.notify();
    }

    pub(super) fn save_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.dialog else { return };
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
                Endpoint::Process { command } => command.split_whitespace().next().unwrap_or("agent").to_string(),
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
        let attention: Vec<(AgentConfig, String)> = configs
            .iter()
            .filter_map(|c| store.needs_attention(&c.id).map(|r| (c.clone(), r)))
            .collect();

        let header = h_flex()
            .gap_3()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .gap_1()
                    .child(div().text_2xl().font_bold().child("Agents"))
                    .child(div().text_sm().text_color(p.muted).child(
                        "Connect local or remote qmtcode instances, inspect their status, and control their runtimes.",
                    )),
            );

        let summary = h_flex()
            .gap_3()
            .child(kit::count_pill(format!("{connected}/{}", configs.len()), "connected", &p))
            .when(!attention.is_empty(), |el| {
                el.child(kit::count_pill(attention.len().to_string(), "need attention", &p))
            });

        let list = if configs.is_empty() {
            v_flex()
                .items_center()
                .gap_2()
                .p_8()
                .rounded(px(14.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .child(kit::icon(IconName::Bot, 22., p.accent))
                .child(div().font_semibold().child("No agents configured"))
                .child(div().text_sm().text_color(p.muted).child(if CAN_SPAWN {
                    "Add a local ACP command, or connect to an ACP WebSocket server on another machine."
                } else {
                    "Connect to an ACP WebSocket server (host:port)."
                }))
                .child(
                    kit::button("empty-add")
                        .primary()
                        .label("Add agent")
                        .on_click(cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx))),
                )
                .into_any_element()
        } else {
            let mut rows = Vec::new();
            for c in configs.iter() {
                rows.push(self.agent_row(c, &p, cx).into_any_element());
            }
            v_flex().gap_2().children(rows).into_any_element()
        };

        let attention_section = (!attention.is_empty()).then(|| {
            v_flex()
                .gap_2()
                .child(
                    v_flex()
                        .child(div().font_semibold().child("Needs attention"))
                        .child(div().text_sm().text_color(p.muted).child("Agents with connection or runtime issues.")),
                )
                .children(attention.into_iter().map(|(c, reason)| {
                    let id = c.id.clone();
                    h_flex()
                        .gap_3()
                        .px_4()
                        .py_3()
                        .rounded(px(12.))
                        .bg(p.warn.opacity(0.08))
                        .border_1()
                        .border_color(p.warn.opacity(0.3))
                        .child(kit::icon(IconName::TriangleAlert, 16., p.warn))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_medium().child(c.name.clone()))
                                .child(div().text_xs().text_color(p.muted).truncate().child(reason)),
                        )
                        .child(
                            kit::button(kit::keyed("attention-retry", id.clone()))
                                .small()
                                .label(if c.is_websocket() { "Reconnect" } else { "Restart" })
                                .on_click(cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.connect(&id, cx)))),
                        )
                }))
        });

        div().w_full().flex().justify_center().px_8().py_8().child(
            v_flex()
                .w_full()
                .max_w(px(1040.))
                .gap_5()
                .child(header)
                .child(summary)
                .child(list)
                .children(attention_section),
        )
    }

    fn agent_row(&self, c: &AgentConfig, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let state = store.state(&c.id);
        let rt = store.runtime(&c.id);
        let sessions = rt.map_or(0, |r| r.sessions.iter().filter(|s| s.root && s.open).count());
        let running = rt.map_or(0, |r| r.sessions.iter().filter(|s| s.root && s.running()).count());
        let models = rt.map_or(0, |r| r.models.len());
        let version = rt.and_then(|r| r.agent_version.clone());
        let host = rt.and_then(|r| r.mesh.as_ref()?.hostname.clone());
        let peers = rt.and_then(|r| r.mesh.as_ref().map(|m| m.peers));
        let is_primary = store.primary_agent().as_deref() == Some(c.id.as_str());
        let live = matches!(state, ConnState::Connected | ConnState::Connecting);
        let connected = state == ConnState::Connected;
        let status = kit::status_color(&state, p);
        let hover_border = p.border_strong;

        let (id_details, id_edit, id_toggle, id_restart, id_auto, id_delete) =
            (c.id.clone(), c.clone(), c.id.clone(), c.id.clone(), c.id.clone(), c.id.clone());
        let websocket = c.is_websocket();

        // Avatar tile with the status dot in its corner.
        let avatar = div()
            .relative()
            .flex_none()
            .size(px(40.))
            .rounded(px(10.))
            .bg(if connected { p.accent_dim } else { p.card })
            .flex()
            .items_center()
            .justify_center()
            .child(kit::icon(if websocket { IconName::Globe } else { IconName::Terminal }, 18., if connected { p.accent } else { p.dim }))
            .child(
                div()
                    .absolute()
                    .right(px(-2.))
                    .bottom(px(-2.))
                    .size(px(12.))
                    .rounded_full()
                    .bg(p.surface)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(kit::dot(status)),
            );

        let title = h_flex()
            .gap_2()
            .child(div().text_sm().font_semibold().truncate().child(c.name.clone()))
            .child(kit::tone_chip(kit::state_label(&state), status))
            .when(is_primary && connected, |el| el.child(kit::tone_chip("default", p.accent)))
            .when(c.host_managed, |el| el.child(kit::chip("host managed", p)));

        let meta = h_flex()
            .gap_1p5()
            .text_xs()
            .text_color(p.dim)
            .child(div().font_family(kit::MONO).truncate().child(c.endpoint_label()))
            .when_some(host, |el, h| el.child("·").child(format!("on {h}")))
            .when_some(version, |el, v| el.child("·").child(div().font_family(kit::MONO).child(format!("v{v}"))))
            .when(c.auto_start && !c.host_managed, |el| el.child("·").child("auto-start"));

        let stat = |value: String, label: &'static str, color: Hsla| {
            v_flex()
                .w(px(68.))
                .items_end()
                .child(div().text_sm().font_semibold().text_color(color).child(value))
                .child(div().text_xs().text_color(p.dim).child(label))
        };
        let stats = connected.then(|| {
            h_flex()
                .flex_none()
                .gap_1()
                .child(stat(sessions.to_string(), "open", p.text))
                .child(stat(running.to_string(), "running", if running > 0 { p.success } else { p.text }))
                .child(stat(models.to_string(), "models", p.text))
                .when_some(peers, |el, n| el.child(stat(n.to_string(), "peers", p.text)))
        });

        // Primary actions always visible; config actions appear on hover.
        let actions = h_flex()
            .gap_0p5()
            .flex_none()
            .pl_2()
            .child(
                kit::icon_button(
                    kit::keyed("toggle", c.id.clone()),
                    if live { IconName::Square } else { IconName::Play },
                    match (live, websocket) {
                        (true, true) => "Disconnect",
                        (true, false) => "Stop",
                        (false, true) => "Connect",
                        (false, false) => "Start",
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |s, cx| if live { s.disconnect(&id_toggle, cx) } else { s.connect(&id_toggle, cx) })
                })),
            )
            .when(live, |el| {
                el.child(
                    kit::icon_button(kit::keyed("restart", c.id.clone()), IconName::RotateCcw, if websocket { "Reconnect" } else { "Restart" })
                        .on_click(cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.connect(&id_restart, cx)))),
                )
            })
            .child(kit::icon_button(kit::keyed("details", c.id.clone()), IconName::Info, "Details").on_click(cx.listener(move |this, _, _, cx| {
                this.details = Some(id_details.clone());
                cx.notify();
            })))
            .when(!c.host_managed, |el| {
                el.child(
                    h_flex()
                        .gap_0p5()
                        .opacity(0.)
                        .group_hover("arow", |s| s.opacity(1.))
                        .child(kit::icon_button(kit::keyed("edit", c.id.clone()), IconName::Pencil, "Edit").on_click(cx.listener(
                            move |this, _, window, cx| this.open_edit_dialog(&id_edit, window, cx),
                        )))
                        .child(
                            kit::icon_button(
                                kit::keyed("auto", c.id.clone()),
                                if c.auto_start { IconName::ToggleRight } else { IconName::ToggleLeft },
                                if c.auto_start { "Disable auto-start" } else { "Enable auto-start" },
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.toggle_auto_start(&id_auto, cx)))),
                        )
                        .child(kit::icon_button(kit::keyed("delete", c.id.clone()), IconName::Trash, "Delete").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.confirm_delete = Some(id_delete.clone());
                                cx.notify();
                            },
                        ))),
                )
            });

        let name_id = c.id.clone();
        h_flex()
            .id(kit::keyed("agent-row", c.id.clone()))
            .group("arow")
            .gap_3()
            .px_4()
            .py_3()
            .rounded(px(14.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .hover(move |el| el.border_color(hover_border))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.details = Some(name_id.clone());
                cx.notify();
            }))
            .child(avatar)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(title)
                    .child(meta)
                    .when_some(
                        match &state {
                            ConnState::Failed(reason) => Some(reason.clone()),
                            _ => None,
                        },
                        |el, reason| el.child(div().text_xs().text_color(p.warn).truncate().child(reason)),
                    ),
            )
            .children(stats)
            .child(actions)
    }

    // ---- overlays -----------------------------------------------------------

    pub(super) fn render_overlay(&mut self, radii: Corners<Pixels>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = *palette(cx);
        let scrim = |id: &'static str| {
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
        };

        if let Some(id) = self.confirm_delete.clone() {
            let name = self.store.read(cx).config(&id).map(|c| c.name.clone()).unwrap_or_default();
            let card = self.modal_card(&p)
                .w(px(420.))
                .child(div().text_lg().font_semibold().child(format!("Delete {name}?")))
                .child(div().text_sm().text_color(p.muted).child(
                    "The configuration is removed from this app. Sessions stay on the agent.",
                ))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(kit::button("del-cancel").label("Cancel").on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_delete = None;
                            cx.notify();
                        })))
                        .child(kit::button("del-ok").danger().label("Delete").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.confirm_delete = None;
                                if this.details.as_deref() == Some(id.as_str()) {
                                    this.details = None;
                                }
                                this.store.update(cx, |s, cx| s.delete_config(&id, cx));
                            },
                        ))),
                );
            return Some(self.centered(scrim("scrim-delete"), card, cx));
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

    fn modal_card(&self, p: &Palette) -> Div {
        v_flex()
            .gap_4()
            .p_5()
            .rounded(px(16.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
    }

    fn centered(&self, scrim: Stateful<Div>, card: Div, cx: &mut Context<Self>) -> AnyElement {
        scrim
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_topmost();
                cx.notify();
            }))
            .child(card.id("modal-card").on_click(|_, _, cx| cx.stop_propagation()))
            .into_any_element()
    }

    fn render_details(&self, id: &str, scrim: Stateful<Div>, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let Some(c) = store.config(id).cloned() else { return div().into_any_element() };
        let state = store.state(id);
        let rt = store.runtime(id);
        let tone = |ok: bool| if ok { p.success } else { p.status_muted };

        let sessions = rt.map_or(0, |r| r.sessions.len());
        let open = rt.map_or(0, |r| r.sessions.iter().filter(|s| s.root && s.open).count());
        let control = rt.and_then(|r| r.control_version());
        let transports = rt.map(|r| r.capability_flags("transport")).unwrap_or_default();
        let features = rt.map(|r| r.capability_flags("features")).unwrap_or_default();
        let methods = rt.map_or(0, |r| r.method_count());
        let logs: Vec<String> = rt.map(|r| r.logs.iter().rev().take(80).cloned().collect()).unwrap_or_default();
        let mesh = rt.and_then(|r| r.mesh.clone());

        let row = |label: &'static str, value: String, mono: bool| {
            h_flex()
                .gap_4()
                .py_1p5()
                .child(div().w(px(120.)).flex_none().text_sm().text_color(p.dim).child(label))
                .child(div().flex_1().min_w_0().text_sm().truncate().when(mono, |el| el.font_family(kit::MONO)).child(value))
        };
        let section = |title: &'static str, desc: &'static str| {
            v_flex()
                .pt_2()
                .child(div().font_semibold().child(title))
                .child(div().text_xs().text_color(p.dim).child(desc))
        };
        let chips = |items: Vec<String>| {
            h_flex().flex_wrap().gap_1().children(items.into_iter().map(|t| kit::chip(t, p)))
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
                            .child(div().text_lg().font_semibold().truncate().child(c.name.clone()))
                            .child(div().text_xs().text_color(p.dim).child(c.transport_label())),
                    )
                    .child(kit::icon_button("details-refresh", IconName::RefreshCw, "Refresh capabilities").on_click(
                        cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.refresh(&refresh_id, cx))),
                    ))
                    .child(kit::icon_button("details-close", IconName::X, "Close").on_click(cx.listener(|this, _, _, cx| {
                        this.details = None;
                        cx.notify();
                    }))),
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
                            .child(kit::stat_card("Connection", kit::state_label(&state), kit::status_color(&state, p), p))
                            .child(kit::stat_card("Sessions", format!("{open} / {sessions}"), p.text, p))
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
                                el.child(row("ACP URL", match &c.endpoint {
                                    Endpoint::WebSocket { address, secure } => {
                                        format!("{}://{address}/acp/ws", if *secure { "wss" } else { "ws" })
                                    }
                                    _ => String::new(),
                                }, true))
                            })
                            .when_some(rt.and_then(|r| r.pid), |el, pid| el.child(row("Process ID", pid.to_string(), true)))
                            .child(row("Agent", rt.and_then(|r| r.agent_title.clone()).unwrap_or("—".into()), false))
                            .child(row("Version", rt.and_then(|r| r.agent_version.clone()).unwrap_or("—".into()), true))
                            .child(row("Auto-start", if c.auto_start { "On".into() } else { "Off".into() }, false))
                            .when_some(mesh, |el, m| {
                                el.child(row("Mesh host", m.hostname.unwrap_or("—".into()), false)).child(row(
                                    "Mesh peers",
                                    format!("{} via {}", m.peers, m.transport.unwrap_or("?".into())),
                                    false,
                                ))
                            }),
                    )
                    .child(section("Capabilities", "Transports and product features reported by the agent."))
                    .child(if control.is_some() {
                        v_flex()
                            .gap_2()
                            .child(h_flex().gap_3().items_start().child(div().w(px(120.)).flex_none().text_sm().text_color(p.dim).child("Transport")).child(chips(transports)))
                            .child(h_flex().gap_3().items_start().child(div().w(px(120.)).flex_none().text_sm().text_color(p.dim).child("Features")).child(chips(features)))
                            .child(row("Methods", methods.to_string(), false))
                            .into_any_element()
                    } else {
                        div().text_sm().text_color(p.dim).child("Not loaded. Connect the agent to read its capabilities.").into_any_element()
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
                    ),
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
