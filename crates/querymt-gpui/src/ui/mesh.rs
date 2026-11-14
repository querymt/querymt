//! Mesh page: status summary, remote nodes, invites (create / copy / revoke).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Page, QmtApp, kit};
use crate::theme::{Palette, palette};
use crate::util::now_ms;

pub const INVITE_TTLS: [(&str, &str); 3] =
    [("1h", "1 hour"), ("24h", "24 hours"), ("168h", "7 days")];

/// `12D3KooWPtRj...unWZUT62`
pub fn short_peer(id: &str) -> String {
    if id.len() <= 24 {
        return id.to_string();
    }
    format!("{}...{}", &id[..12], &id[id.len() - 8..])
}

fn epoch_ms(t: f64) -> f64 {
    if t > 1e12 { t } else { t * 1000. }
}

fn expires_label(t: f64) -> String {
    if t <= 0. {
        return "no expiry".into();
    }
    let secs = (epoch_ms(t) - now_ms()) / 1000.;
    if secs <= 0. {
        return "expired".into();
    }
    let secs = secs as u64;
    match secs {
        0..3600 => format!("expires in {}m", secs / 60),
        3600..172800 => format!("expires in {}h", secs / 3600),
        _ => format!("expires in {}d", secs / 86400),
    }
}

pub fn section_title(title: &'static str, desc: &'static str, p: &Palette) -> Div {
    v_flex()
        .gap_0p5()
        .child(div().font_semibold().child(title))
        .child(div().text_sm().text_color(p.muted).child(desc))
}

pub fn panel(p: &Palette) -> Div {
    v_flex()
        .rounded(px(14.))
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
}

fn copy_button(id: impl Into<ElementId>, text: String, tooltip: &'static str) -> impl IntoElement {
    kit::icon_button(id, IconName::Copy, tooltip)
        .xsmall()
        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
}

impl QmtApp {
    pub(super) fn render_mesh(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let store = self.store.read(cx);
        let agent = store.primary_agent();
        let agent_name = agent
            .as_deref()
            .and_then(|a| store.config(a))
            .map(|c| c.name.clone());
        let rt = agent.as_deref().and_then(|a| store.runtime(a));
        let refreshing = agent
            .as_deref()
            .is_some_and(|a| store.is_pending(a, "extras"));
        let mesh_feature = rt
            .and_then(|r| r.capabilities.as_ref())
            .and_then(|c| c.pointer("/features/mesh"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mesh = rt.and_then(|r| r.mesh.clone());
        let nodes = rt.map(|r| r.mesh_nodes.clone()).unwrap_or_default();
        let invites = rt.map(|r| r.mesh_invites.clone()).unwrap_or_default();
        let last_invite = rt.and_then(|r| r.last_invite.clone());
        let error = rt.and_then(|r| r.action_error.clone());
        let creating = agent
            .as_deref()
            .is_some_and(|a| store.is_pending(a, "invite"));
        let revoking: Vec<String> = invites
            .iter()
            .filter(|i| {
                agent
                    .as_deref()
                    .is_some_and(|a| store.is_pending(a, &format!("revoke:{}", i.id)))
            })
            .map(|i| i.id.clone())
            .collect();

        let header = h_flex()
            .gap_3()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .gap_1()
                    .child(div().text_2xl().font_bold().child("Mesh"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(p.muted)
                            .child("Manage remote nodes and invites."),
                    ),
            )
            .when_some(agent_name.clone(), |el, name| {
                el.child(kit::chip(format!("via {name}"), &p))
            })
            .when_some(agent.clone(), |el, a| {
                el.child(
                    kit::loading_icon_button(
                        "mesh-refresh",
                        IconName::RefreshCw,
                        "Refresh mesh",
                        refreshing,
                        24.,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |s, cx| s.refresh_extras(&a, cx))
                    })),
                )
            });

        let page = |body: Vec<AnyElement>| {
            div().w_full().flex().justify_center().px_8().py_6().child(
                v_flex()
                    .w_full()
                    .max_w(px(1040.))
                    .gap_5()
                    .child(header)
                    .children(body),
            )
        };

        let Some(agent) = agent else {
            let empty = panel(&p)
                .p_6()
                .items_center()
                .gap_2()
                .child(kit::icon(IconName::Network, 22., p.accent))
                .child(div().font_semibold().child("No agent connected"))
                .child(
                    div()
                        .text_sm()
                        .text_color(p.muted)
                        .child("Mesh status comes from a connected agent."),
                )
                .child(
                    kit::button("mesh-agents")
                        .primary()
                        .label("Open Agents")
                        .on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx))),
                );
            return page(vec![empty.into_any_element()]);
        };
        if !mesh_feature {
            let empty =
                panel(&p)
                    .p_6()
                    .items_center()
                    .gap_2()
                    .child(kit::icon(IconName::Network, 22., p.dim))
                    .child(
                        div()
                            .font_semibold()
                            .child("Mesh is not enabled on this agent"),
                    )
                    .child(div().text_sm().text_color(p.muted).child(
                        "Start qmtcode with mesh support to share sessions across machines.",
                    ));
            return page(vec![empty.into_any_element()]);
        }

        // ---- summary
        let enabled = mesh.as_ref().is_some_and(|m| m.enabled);
        let active_sessions: u64 = nodes.iter().map(|n| n.active_sessions).sum();
        let active_invites = invites.iter().filter(|i| i.status == "active").count();
        let cell = |label: &'static str, value: String, color: Hsla, mono: bool, last: bool| {
            v_flex()
                .flex_1()
                .px_4()
                .py_3()
                .gap_0p5()
                .when(!last, |el| el.border_r_1().border_color(p.border))
                .child(div().text_xs().text_color(p.dim).child(label))
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .text_color(color)
                        .when(mono, |el| el.font_family(kit::MONO))
                        .child(value),
                )
        };
        let peer_id = mesh
            .as_ref()
            .and_then(|m| m.peer_id.clone())
            .unwrap_or_default();
        let relays = mesh.as_ref().map(|m| m.relays.clone()).unwrap_or_default();
        let meta = |label: &'static str, value: String| {
            h_flex()
                .gap_1p5()
                .child(div().font_semibold().text_color(p.muted).child(label))
                .child(div().font_family(kit::MONO).child(value))
        };
        let summary = panel(&p)
            .overflow_hidden()
            .child(
                h_flex()
                    .child(cell(
                        "Status",
                        if enabled {
                            "Enabled".into()
                        } else {
                            "Disabled".into()
                        },
                        if enabled { p.success } else { p.status_muted },
                        true,
                        false,
                    ))
                    .child(cell("Nodes", nodes.len().to_string(), p.text, false, false))
                    .child(cell(
                        "Active sessions",
                        active_sessions.to_string(),
                        p.text,
                        false,
                        false,
                    ))
                    .child(cell(
                        "Active invites",
                        active_invites.to_string(),
                        p.text,
                        false,
                        true,
                    )),
            )
            .child(
                h_flex()
                    .gap_4()
                    .px_4()
                    .py_2()
                    .bg(p.card)
                    .border_t_1()
                    .border_color(p.border)
                    .text_xs()
                    .text_color(p.dim)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(meta("Peer", short_peer(&peer_id)))
                            .when(!peer_id.is_empty(), |el| {
                                el.child(copy_button("copy-peer", peer_id.clone(), "Copy peer id"))
                            }),
                    )
                    .when_some(mesh.as_ref().and_then(|m| m.hostname.clone()), |el, h| {
                        el.child(meta("Host", h))
                    })
                    .child(meta(
                        "Transport",
                        mesh.as_ref()
                            .and_then(|m| m.transport.clone())
                            .unwrap_or("—".into()),
                    ))
                    .child(meta(
                        "Known peers",
                        mesh.as_ref().map_or(0, |m| m.peers).to_string(),
                    ))
                    .children(relays.into_iter().take(2).map(|(url, ok)| {
                        let host = url
                            .trim_start_matches("https://")
                            .trim_end_matches(['/', '.'])
                            .to_string();
                        h_flex()
                            .gap_1p5()
                            .child(kit::dot(if ok { p.success } else { p.warn }).size(px(6.)))
                            .child(div().font_semibold().text_color(p.muted).child("Relay"))
                            .child(div().font_family(kit::MONO).child(host))
                    })),
            );

        // ---- nodes
        let nodes_list = if nodes.is_empty() {
            panel(&p)
                .p_4()
                .text_sm()
                .text_color(p.dim)
                .child("No remote nodes yet. Invite a peer to join this mesh.")
        } else {
            panel(&p).p_1().children(nodes.into_iter().map(|n| {
                let hover = p.card;
                h_flex()
                    .id(kit::keyed("node", n.id.clone()))
                    .group("node")
                    .gap_3()
                    .px_3()
                    .py_2p5()
                    .rounded(px(10.))
                    .hover(move |s| s.bg(hover))
                    .child(kit::identicon(&n.id, 22.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().text_sm().font_semibold().child(
                                        if n.label.is_empty() {
                                            short_peer(&n.id)
                                        } else {
                                            n.label.clone()
                                        },
                                    ))
                                    .child(if n.active_sessions > 0 {
                                        kit::tone_chip(
                                            format!("{} active", n.active_sessions),
                                            p.success,
                                        )
                                    } else {
                                        kit::chip("idle", &p)
                                    })
                                    .children(
                                        n.capabilities.iter().map(|c| kit::chip(c.clone(), &p)),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(p.dim)
                                    .child(div().font_family(kit::MONO).child(short_peer(&n.id)))
                                    .when(
                                        !n.transport.is_empty() && n.transport != "unknown",
                                        |el| el.child("·").child(n.transport.clone()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .opacity(0.)
                            .group_hover("node", |s| s.opacity(1.))
                            .child(copy_button(
                                kit::keyed("copy-node", n.id.clone()),
                                n.id.clone(),
                                "Copy peer id",
                            )),
                    )
            }))
        };

        // ---- invites
        let ttl = self.invite_ttl;
        let form = self.invite_form.then(|| {
            let a = agent.clone();
            panel(&p)
                .p_4()
                .gap_3()
                .child(div().font_semibold().text_sm().child("New invite"))
                .child(
                    h_flex()
                        .gap_6()
                        .items_end()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_medium()
                                        .text_color(p.muted)
                                        .child("Expires after"),
                                )
                                .child(kit::segment_bar(&p).children(
                                    INVITE_TTLS.iter().enumerate().map(|(ix, (v, label))| {
                                        let v: &'static str = v;
                                        kit::segment(("ttl", ix), ttl == v, &p)
                                            .label(*label)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.invite_ttl = v;
                                                cx.notify();
                                            }))
                                    }),
                                )),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_medium()
                                        .text_color(p.muted)
                                        .child("Max uses"),
                                )
                                .child(Input::new(&self.invite_uses).small().w(px(130.))),
                        )
                        .child(div().flex_1())
                        .child(
                            kit::button("invite-cancel")
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.invite_form = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            kit::button("invite-create")
                                .small()
                                .primary()
                                .label("Create invite")
                                .map(|button| {
                                    kit::loading_button(button, "invite-spinner", creating, 12.)
                                })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.create_invite(&a, cx)),
                                ),
                        ),
                )
        });
        let created = last_invite.map(|url| {
            let a = agent.clone();
            h_flex()
                .gap_3()
                .px_4()
                .py_3()
                .rounded(px(12.))
                .bg(p.success.opacity(0.08))
                .border_1()
                .border_color(p.success.opacity(0.3))
                .child(kit::icon(IconName::Ticket, 16., p.success))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_semibold().child("Invite created"))
                        .child(
                            div()
                                .text_xs()
                                .font_family(kit::MONO)
                                .text_color(p.muted)
                                .truncate()
                                .child(url.clone()),
                        ),
                )
                .child(
                    kit::button("copy-invite")
                        .small()
                        .icon(IconName::Copy)
                        .label("Copy link")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
                        }),
                )
                .child(
                    kit::icon_button("dismiss-invite", IconName::X, "Dismiss").on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.dismiss_invite(&a, cx))
                        }),
                    ),
                )
        });
        let invites_list = if invites.is_empty() {
            panel(&p)
                .p_4()
                .text_sm()
                .text_color(p.dim)
                .child("No active invites. Create one when another peer needs access.")
        } else {
            panel(&p).p_1().children(invites.into_iter().map(|i| {
                let a = agent.clone();
                let id = i.id.clone();
                let active = i.status == "active";
                h_flex()
                    .gap_3()
                    .px_3()
                    .py_2p5()
                    .child(kit::icon(
                        IconName::Ticket,
                        16.,
                        if active { p.accent } else { p.dim },
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().text_sm().font_semibold().child(
                                        i.mesh_name.clone().unwrap_or_else(|| "Mesh invite".into()),
                                    ))
                                    .child(kit::tone_chip(
                                        i.status.clone(),
                                        if active { p.success } else { p.status_muted },
                                    )),
                            )
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(p.dim)
                                    .child(div().font_family(kit::MONO).child(short_peer(&i.id)))
                                    .child("·")
                                    .child(if i.max_uses == 0 {
                                        "unlimited uses".to_string()
                                    } else {
                                        format!("{} of {} uses left", i.uses_remaining, i.max_uses)
                                    })
                                    .child("·")
                                    .child(expires_label(i.expires_at))
                                    .when(i.created_at > 0., |el| el),
                            ),
                    )
                    .when(active, |el| {
                        el.child(
                            kit::button(kit::keyed("revoke", id.clone()))
                                .xsmall()
                                .ghost()
                                .label("Revoke")
                                .map(|button| {
                                    kit::loading_button(
                                        button,
                                        kit::keyed("revoke-spinner", id.clone()),
                                        revoking.contains(&id),
                                        12.,
                                    )
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store.update(cx, |s, cx| s.revoke_invite(&a, &id, cx))
                                })),
                        )
                    })
            }))
        };

        let error_banner = error.map(|err| {
            let a = agent.clone();
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
                    kit::icon_button("mesh-err", IconName::X, "Dismiss").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.clear_action_error(&a, cx))
                        },
                    )),
                )
        });

        let invites_header = h_flex()
            .items_end()
            .child(
                section_title("Invites", "Access links for peers joining this mesh.", &p).flex_1(),
            )
            .when(!self.invite_form, |el| {
                el.child(
                    kit::button("new-invite")
                        .small()
                        .icon(IconName::Ticket)
                        .label("New invite")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.invite_form = true;
                            cx.notify();
                        })),
                )
            });

        page(vec![
            summary.into_any_element(),
            v_flex()
                .gap_3()
                .child(section_title(
                    "Nodes",
                    "Remote peers available through the selected agent.",
                    &p,
                ))
                .child(nodes_list)
                .into_any_element(),
            v_flex()
                .gap_3()
                .child(invites_header)
                .children(error_banner)
                .children(form)
                .children(created)
                .child(invites_list)
                .into_any_element(),
        ])
    }

    fn create_invite(&mut self, agent: &str, cx: &mut Context<Self>) {
        let uses = self
            .invite_uses
            .read(cx)
            .value()
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0);
        let ttl = self.invite_ttl;
        self.invite_form = false;
        self.store
            .update(cx, |s, cx| s.create_invite(agent, ttl, uses, None, cx));
        cx.notify();
    }
}
