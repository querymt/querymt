//! Mesh page: status summary, remote nodes, invites (create / copy / revoke).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex};
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

pub fn panel(p: &Palette) -> Div {
    v_flex()
        .rounded(px(14.))
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
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

        let show_remote = store.prefs.show_remote_sessions;
        let remote_peers = store.prefs.remote_session_peers.clone();
        let header = kit::page_header(
            "Mesh",
            agent_name
                .clone()
                .map(|name| kit::chip(format!("via {name}"), &p).into_any_element()),
            Some("Manage remote nodes and invites."),
            agent
                .clone()
                .map(|a| {
                    kit::loading_icon_button(
                        "mesh-refresh",
                        IconName::RefreshCw,
                        "Refresh mesh",
                        refreshing,
                        24.,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |s, cx| s.refresh_extras(&a, cx))
                    }))
                    .into_any_element()
                })
                .into_iter()
                .collect(),
            &p,
        );

        let page = |body: Vec<AnyElement>| {
            kit::page(std::iter::once(header.into_any_element()).chain(body))
        };

        let Some(agent) = agent else {
            let empty = kit::card(&p)
                .p_6()
                .items_center()
                .gap_2()
                .child(kit::icon_tile(IconName::Network, false, &p))
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
                kit::card(&p)
                    .p_6()
                    .items_center()
                    .gap_2()
                    .child(kit::icon_tile(IconName::Network, false, &p))
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
        // Tiles on the card colour, set apart by gaps rather than borders.
        let cell = |label: &'static str, value: String, color: Hsla, mono: bool, _last: bool| {
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
        let summary = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
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
                    .flex_wrap()
                    .gap_x_4()
                    .gap_y_1()
                    .px_4()
                    .py_2()
                    .rounded(px(12.))
                    .bg(p.card)
                    .text_xs()
                    .text_color(p.muted)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(meta("Peer", short_peer(&peer_id)))
                            .when(!peer_id.is_empty(), |el| {
                                el.child(kit::copy_button(
                                    "copy-peer",
                                    peer_id.clone(),
                                    "Copy peer ID",
                                ))
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
        // Listing remote sessions: on here, then per peer below.
        let remote_card = kit::card(&p).child(
            kit::card_row(true, &p)
                .child(kit::icon_tile(IconName::MessagesSquare, show_remote, &p))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(13.4))
                                .font_semibold()
                                .child("Show remote sessions"),
                        )
                        .child(
                            div()
                                .mt(px(3.2))
                                .text_xs()
                                .text_color(p.muted)
                                .child(if show_remote {
                                    "Sessions of the peers you include below appear in Sessions."
                                } else {
                                    "List sessions owned by other machines in this mesh alongside local ones."
                                }),
                        ),
                )
                .child(
                    kit::switch("mesh-show-remote")
                        .checked(show_remote)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            let v = *v;
                            this.store
                                .update(cx, |s, cx| s.set_show_remote_sessions(v, cx));
                        })),
                ),
        );

        let nodes_list = if nodes.is_empty() {
            kit::card(&p)
                .p_4()
                .text_sm()
                .text_color(p.muted)
                .child("No remote nodes yet. Invite a peer to join this mesh.")
        } else {
            kit::card(&p)
                .children(nodes.into_iter().enumerate().map(|(ix, n)| {
                    let (_, hover) = kit::subtle(&p);
                    let included = remote_peers.contains(&n.id);
                    let name = if n.label.is_empty() {
                        short_peer(&n.id)
                    } else {
                        n.label.clone()
                    };
                    let node_id = n.id.clone();
                    let include = h_flex().flex_none().gap_2().child(
                        Switch::new(kit::keyed("node-include", n.id.clone()))
                            .when(show_remote, |s| s.cursor_pointer())
                            .checked(included)
                            .small()
                            .label("Include")
                            .disabled(!show_remote)
                            .tooltip(if show_remote {
                                format!("Show sessions of {name} in Sessions")
                            } else {
                                "Turn on Show remote sessions first".to_string()
                            })
                            .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                let v = *v;
                                this.store
                                    .update(cx, |s, cx| s.set_remote_session_peer(&node_id, v, cx));
                            })),
                    );
                    kit::card_row(ix == 0, &p)
                        .id(kit::keyed("node", n.id.clone()))
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
                                        .text_color(p.muted)
                                        .child(
                                            div().font_family(kit::MONO).child(short_peer(&n.id)),
                                        )
                                        .child(kit::copy_button(
                                            kit::keyed("copy-node", n.id.clone()),
                                            n.id.clone(),
                                            "Copy peer ID",
                                        ))
                                        .when(
                                            !n.transport.is_empty() && n.transport != "unknown",
                                            |el| el.child("·").child(n.transport.clone()),
                                        ),
                                ),
                        )
                        .child(include)
                }))
                .child(kit::card_mask(&p))
        };

        // ---- invites
        // The new link: copy it (with feedback), or expand it into a QR code
        // to scan on the other machine.
        let qr_open = self.invite_qr;
        let link_copied = self.copied.as_deref() == Some("mesh-invite-link");
        let created = last_invite.map(|url| {
            let a = agent.clone();
            let copy_url = url.clone();
            kit::card(&p)
                .bg(p.success.opacity(0.08))
                .child(
                    kit::card_row(true, &p)
                        .child(
                            div()
                                .flex_none()
                                .size(px(34.4))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(8.))
                                .bg(p.success.opacity(0.14))
                                .child(kit::icon(IconName::CircleCheck, 17., p.success)),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(13.4))
                                        .font_semibold()
                                        .child("Invite created"),
                                )
                                .child(
                                    div()
                                        .mt(px(3.2))
                                        .text_xs()
                                        .font_family(kit::MONO)
                                        .text_color(p.muted)
                                        .truncate()
                                        .child(url.clone()),
                                ),
                        )
                        .child(
                            kit::flat_icon_btn("dismiss-invite", IconName::X, "Dismiss", &p)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.invite_qr = false;
                                    this.store.update(cx, |s, cx| s.dismiss_invite(&a, cx))
                                })),
                        )
                        .child(
                            kit::flat_btn("copy-invite", true, true, &p)
                                .child(kit::icon(
                                    if link_copied {
                                        IconName::Check
                                    } else {
                                        IconName::Copy
                                    },
                                    14.,
                                    if link_copied { p.success } else { p.text },
                                ))
                                .child(if link_copied { "Copied" } else { "Copy link" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_url.clone(),
                                    ));
                                    this.flash_copied("mesh-invite-link".into(), cx);
                                })),
                        )
                        .child(
                            // Last: it expands the card, the chevron says so.
                            kit::flat_btn("invite-qr", true, true, &p)
                                .child(kit::icon(IconName::QrCode, 14., p.text))
                                .child("QR code")
                                .child(kit::icon(
                                    if qr_open {
                                        IconName::ChevronUp
                                    } else {
                                        IconName::ChevronDown
                                    },
                                    14.,
                                    p.muted,
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.invite_qr = !this.invite_qr;
                                    cx.notify();
                                })),
                        ),
                )
                .when(qr_open, |el| {
                    el.child(
                        v_flex()
                            .items_center()
                            .gap_2()
                            .pb_4()
                            .children(kit::qr_code(&url, 208.).map(|qr| {
                                div().p_2().rounded(px(10.)).bg(gpui_kit::white()).child(qr)
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .child("Scan on the machine that should join this mesh."),
                            ),
                    )
                })
        });
        let invites_list = if invites.is_empty() {
            kit::card(&p)
                .p_4()
                .text_sm()
                .text_color(p.muted)
                .child("No active invites. Create one when another peer needs access.")
        } else {
            kit::card(&p)
                .children(invites.into_iter().enumerate().map(|(ix, i)| {
                    let a = agent.clone();
                    let id = i.id.clone();
                    // The agent lists invites that can still be used as "pending".
                    let active = matches!(i.status.as_str(), "pending" | "active");
                    let (_, hover) = kit::subtle(&p);
                    kit::card_row(ix == 0, &p)
                        .id(kit::keyed("invite", id.clone()))
                        .hover(move |s| s.bg(hover))
                        .child(kit::icon_tile(IconName::Ticket, active, &p))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div().text_sm().font_semibold().child(
                                                i.mesh_name
                                                    .clone()
                                                    .unwrap_or_else(|| "Mesh invite".into()),
                                            ),
                                        )
                                        .child(kit::tone_chip(
                                            i.status.clone(),
                                            if active { p.success } else { p.status_muted },
                                        )),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(p.muted)
                                        .child(
                                            div().font_family(kit::MONO).child(short_peer(&i.id)),
                                        )
                                        .child(kit::copy_button(
                                            kit::keyed("copy-invite-id", id.clone()),
                                            id.clone(),
                                            "Copy invite ID",
                                        ))
                                        .child("·")
                                        .child(if i.max_uses == 0 {
                                            "unlimited uses".to_string()
                                        } else {
                                            format!(
                                                "{} of {} uses left",
                                                i.uses_remaining, i.max_uses
                                            )
                                        })
                                        .child("·")
                                        .child(expires_label(i.expires_at))
                                        .when(i.created_at > 0., |el| el),
                                ),
                        )
                        .when(active, |el| {
                            let pending = revoking.contains(&id);
                            el.child(
                                kit::icon_button(
                                    kit::keyed("revoke", id.clone()),
                                    IconName::Trash,
                                    "Revoke invite",
                                )
                                .loading(pending)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.store.update(cx, |s, cx| s.revoke_invite(&a, &id, cx))
                                    },
                                )),
                            )
                        })
                }))
                .child(kit::card_mask(&p))
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

        let invites_header = kit::section_header(
            "Invites",
            "Access links for peers joining this mesh.",
            (!self.invite_form).then(|| {
                kit::flat_btn("new-invite", !creating, true, &p)
                    .child(kit::icon(IconName::Plus, 14., p.text))
                    .child("New invite")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.invite_form = true;
                        cx.notify();
                    }))
                    .into_any_element()
            }),
            &p,
        );

        page(vec![
            summary.into_any_element(),
            v_flex()
                .gap_3()
                .child(kit::section_header(
                    "Remote sessions",
                    "Sessions owned by other machines in this mesh.",
                    None,
                    &p,
                ))
                .child(remote_card)
                .into_any_element(),
            v_flex()
                .gap_3()
                .child(kit::section_header(
                    "Nodes",
                    "Remote peers available through the selected agent.",
                    None,
                    &p,
                ))
                .child(nodes_list)
                .into_any_element(),
            v_flex()
                .gap_3()
                .child(invites_header)
                .children(error_banner)
                .children(created)
                .child(invites_list)
                .into_any_element(),
        ])
    }

    pub(super) fn create_invite(&mut self, agent: &str, cx: &mut Context<Self>) {
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
        self.invite_qr = false;
        self.store
            .update(cx, |s, cx| s.create_invite(agent, ttl, uses, None, cx));
        cx.notify();
    }
}
