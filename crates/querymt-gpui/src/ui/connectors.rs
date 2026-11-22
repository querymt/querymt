//! Settings → Connectors (desktop `McpServerList`): sign in to the agent's
//! OAuth-protected MCP servers. They share the agent's `_querymt/auth/*`
//! methods with model providers (auth id `mcp:<server>`) but are listed
//! here, apart from the providers.

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::kit::{accent_btn, flat_btn, subtle};
use super::mesh::panel;
use super::{QmtApp, kit};
use crate::store::{OAuthFlow, ProviderAuth};
use crate::theme::Palette;

fn signed_in(server: &ProviderAuth) -> bool {
    server.oauth_status.as_deref() == Some("connected")
}

/// Servers to sign in to first, then by name (desktop order).
fn sorted(mut servers: Vec<ProviderAuth>) -> Vec<ProviderAuth> {
    servers.sort_by(|a, b| {
        signed_in(a).cmp(&signed_in(b)).then_with(|| {
            a.mcp_server()
                .to_lowercase()
                .cmp(&b.mcp_server().to_lowercase())
        })
    });
    servers
}

impl QmtApp {
    /// Finish a running OAuth flow of agent `agent` in place: paste the
    /// callback (redirect flows) or confirm the device approval.
    pub(super) fn oauth_flow_box(
        &self,
        agent: &str,
        flow: &OAuthFlow,
        key: SharedString,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let (a, a2) = (agent.to_string(), agent.to_string());
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(8.))
            .bg(subtle(p).1)
            .child(div().text_size(px(11.8)).text_color(p.muted).child(if flow.needs_code {
                "A browser window opened. After approving, paste the callback URL or code here."
            } else {
                "A browser window opened. Approve the device there, then press Done."
            }))
            .child(
                h_flex()
                    .gap_2()
                    .when(flow.needs_code, |el| {
                        el.child(Input::new(&self.oauth_code).small().flex_1())
                    })
                    .when(!flow.needs_code, |el| el.child(div().flex_1()))
                    .child(
                        flat_btn(kit::keyed("mcp-oauth-cancel", key.clone()), true, true, p)
                            .child("Cancel")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| s.cancel_oauth(&a, cx))
                            })),
                    )
                    .child(
                        accent_btn(kit::keyed("mcp-oauth-done", key), true, true, p)
                            .child("Done")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let code = this.oauth_code.read(cx).value().trim().to_string();
                                this.oauth_code.update(cx, |s, cx| {
                                    s.set_value(SharedString::default(), window, cx)
                                });
                                this.store.update(cx, |s, cx| s.complete_oauth(&a2, code, cx));
                            })),
                    ),
            )
    }

    /// Above the composer: MCP servers this session left out, with sign-in
    /// (desktop `session-mcp-auth-notice`). Hidden for read-only sessions.
    pub(super) fn render_mcp_notice(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (agent, cwd, oauth, skipped) = {
            let store = self.store.read(cx);
            let view = store.session.as_ref().filter(|v| v.access.is_none())?;
            let (agent, cwd) = (view.agent_id.clone(), view.cwd.clone());
            let skipped: Vec<_> = store
                .skipped_mcp()
                .into_iter()
                .map(|s| {
                    let signed_in = store.mcp_signed_in(&agent, &s.provider);
                    let busy = store.is_pending(&agent, &format!("oauth:{}", s.provider));
                    (s, signed_in, busy)
                })
                .collect();
            let oauth = store.runtime(&agent).and_then(|rt| rt.oauth.clone());
            (agent, cwd, oauth, skipped)
        };
        if skipped.is_empty() {
            return None;
        }
        let rows: Vec<AnyElement> = skipped
            .iter()
            .map(|(server, signed_in, busy)| {
                let (signed_in, busy) = (*signed_in, *busy);
                let key: SharedString = server.server.clone().into();
                let signing = oauth.as_ref().filter(|f| f.provider == server.provider);
                let text = if signed_in {
                    format!(
                        "Signed in to \"{}\". Start a new session to use its tools.",
                        server.server
                    )
                } else {
                    server.describe()
                };
                let tone = if server.error.is_some() {
                    p.warn
                } else {
                    p.accent
                };
                let actions = h_flex()
                    .flex_none()
                    .gap_1p5()
                    .when(signed_in, |el| {
                        let (agent, cwd) = (agent.clone(), cwd.clone());
                        el.child(
                            accent_btn(kit::keyed("mcp-notice-new", key.clone()), true, true, p)
                                .child("New session")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.new_session_in(cwd.clone(), agent.clone(), window, cx)
                                })),
                        )
                    })
                    .when(!signed_in, |el| {
                        let enabled = signing.is_none() && oauth.is_none() && !busy;
                        let (agent, provider) = (agent.clone(), server.provider.clone());
                        let selector = format!("mcp-notice-signin-{}", server.server);
                        el.child(
                            flat_btn(
                                kit::keyed("mcp-notice-signin", key.clone()),
                                enabled,
                                true,
                                p,
                            )
                            .debug_selector(move || selector)
                            .child(kit::icon(IconName::LogIn, 14., p.text))
                            .child(if server.error.is_some() {
                                "Retry sign-in"
                            } else {
                                "Sign in"
                            })
                            .when(enabled, |el| {
                                el.on_click(cx.listener(move |this, _, _, cx| {
                                    this.store
                                        .update(cx, |s, cx| s.start_oauth(&agent, &provider, cx))
                                }))
                            }),
                        )
                        .when(server.error.is_some(), |el| {
                            el.child(
                                flat_btn(
                                    kit::keyed("mcp-notice-settings", key.clone()),
                                    true,
                                    true,
                                    p,
                                )
                                .child("Settings")
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.settings_tab =
                                            super::settings::SettingsTab::Connectors;
                                        this.go(super::Page::Settings, cx);
                                    },
                                )),
                            )
                        })
                    })
                    .child({
                        let name = server.server.clone();
                        kit::icon_button(
                            kit::keyed("mcp-notice-dismiss", key.clone()),
                            IconName::X,
                            "Dismiss",
                        )
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |s, cx| s.dismiss_skipped_mcp(&name, cx))
                        }))
                    });
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2p5()
                            .child(kit::icon(
                                if signed_in {
                                    IconName::CircleCheck
                                } else {
                                    IconName::Plug
                                },
                                15.,
                                if signed_in { p.success } else { tone },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .text_color(p.text)
                                    .child(text),
                            )
                            .child(actions),
                    )
                    .children(
                        signing.map(|flow| self.oauth_flow_box(&agent, flow, key.clone(), p, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        Some(
            v_flex()
                .id("mcp-notice")
                .debug_selector(|| "mcp-notice".into())
                .w_full()
                .gap_2()
                .px_3()
                .py_2p5()
                .rounded(px(12.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .children(rows)
                .into_any_element(),
        )
    }

    /// The primary agent has MCP servers to sign in to: the tab is shown.
    pub(super) fn has_connectors(&self, cx: &App) -> bool {
        let store = self.store.read(cx);
        store
            .primary_agent()
            .and_then(|a| store.runtime(&a))
            .is_some_and(|rt| !rt.mcp_servers.is_empty())
    }

    pub(super) fn settings_connectors(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let note = |text: &'static str, p: &Palette| {
            panel(p)
                .p_4()
                .text_sm()
                .text_color(p.muted)
                .child(text)
                .into_any_element()
        };
        let Some((agent, rt)) = store
            .primary_agent()
            .and_then(|a| store.runtime(&a).map(|rt| (a, rt)))
        else {
            return note(
                "The agent is not running, so connectors cannot be shown. Open Agents to start it or see why it failed.",
                p,
            );
        };
        let servers = sorted(rt.mcp_servers.clone());
        let oauth = rt.oauth.clone();
        let error = rt.action_error.clone();
        let pending: Vec<bool> = servers
            .iter()
            .map(|s| store.is_pending(&agent, &format!("oauth:{}", s.provider)))
            .collect();
        let loading = store.is_pending(&agent, "extras");
        let sign_in_locked = oauth.is_some();
        if servers.is_empty() {
            return if loading {
                note("Checking connectors…", p)
            } else {
                note(
                    "No OAuth connectors are configured for this agent. Add auth = { type = \"oauth\" } to an HTTP [[mcp]] entry.",
                    p,
                )
            };
        }
        let (subtle_border, _) = subtle(p);
        let rows: Vec<AnyElement> = servers
            .iter()
            .enumerate()
            .map(|(ix, server)| {
                let key: SharedString = server.provider.clone().into();
                let connected = signed_in(server);
                let busy = pending[ix];
                let signing = oauth
                    .as_ref()
                    .is_some_and(|f| f.provider == server.provider);
                let (a, prov) = (agent.clone(), server.provider.clone());
                let action: AnyElement = if signing {
                    let a = agent.clone();
                    h_flex()
                        .gap_1()
                        .child(
                            flat_btn(kit::keyed("mcp-signing", key.clone()), false, true, p)
                                .child(kit::spinner(
                                    kit::keyed("mcp-spin", key.clone()),
                                    14.,
                                    p.muted,
                                ))
                                .child("Signing in…"),
                        )
                        .child(
                            kit::icon_button(
                                kit::keyed("mcp-cancel", key.clone()),
                                IconName::CircleStop,
                                "Cancel sign-in",
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.store.update(cx, |s, cx| s.cancel_oauth(&a, cx))
                                },
                            )),
                        )
                        .into_any_element()
                } else if connected {
                    flat_btn(kit::keyed("mcp-signout", key.clone()), !busy, true, p)
                        .child(kit::icon(IconName::LogOut, 14., p.text))
                        .child("Sign out")
                        .when(!busy, |el| {
                            el.on_click(cx.listener(move |this, _, _, cx| {
                                this.store
                                    .update(cx, |s, cx| s.logout_provider(&a, &prov, cx))
                            }))
                        })
                        .into_any_element()
                } else {
                    let enabled = !busy && !sign_in_locked;
                    let selector = format!("mcp-signin-{}", server.mcp_server());
                    flat_btn(kit::keyed("mcp-signin", key.clone()), enabled, true, p)
                        .debug_selector(move || selector)
                        .child(kit::icon(IconName::LogIn, 14., p.text))
                        .child("Sign in")
                        .when(enabled, |el| {
                            el.on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| s.start_oauth(&a, &prov, cx))
                            }))
                        })
                        .into_any_element()
                };
                // Finish the OAuth flow in place, like a provider's.
                let flow = oauth.as_ref().filter(|_| signing).map(|flow| {
                    self.oauth_flow_box(&agent, flow, key.clone(), p, cx)
                        .mx(px(12.))
                        .mb(px(12.))
                });
                v_flex()
                    .id(kit::keyed("mcp-row", key.clone()))
                    .min_w_0()
                    .when(ix > 0, |el| el.border_t_1().border_color(subtle_border))
                    .child(
                        h_flex()
                            .relative()
                            .min_w_0()
                            .gap_3()
                            .pl(px(18.))
                            .pr(px(10.))
                            .py(px(12.))
                            // Signed in: the providers' leading status bar.
                            .when(connected, |el| {
                                el.child(
                                    div()
                                        .absolute()
                                        .left(px(7.))
                                        .top(px(14.))
                                        .bottom(px(14.))
                                        .w(px(3.))
                                        .rounded_full()
                                        .bg(p.success),
                                )
                            })
                            .child(kit::icon(
                                IconName::Plug,
                                15.,
                                if connected { p.success } else { p.muted },
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(13.4))
                                            .font_semibold()
                                            .text_color(p.text)
                                            .child(server.mcp_server().to_string()),
                                    )
                                    .child(
                                        div()
                                            .mt(px(3.2))
                                            .text_size(px(12.))
                                            .text_color(p.muted)
                                            .child(if connected {
                                                "Signed in"
                                            } else {
                                                "Sign-in required"
                                            }),
                                    ),
                            )
                            .child(action),
                    )
                    .children(flow)
                    .into_any_element()
            })
            .collect();
        v_flex()
            .gap(px(12.8))
            .when_some(error, |el, err| {
                let a = agent.clone();
                el.child(
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
                            kit::icon_button("mcp-err", IconName::X, "Dismiss").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.store.update(cx, |s, cx| s.clear_action_error(&a, cx))
                                }),
                            ),
                        ),
                )
            })
            .child(
                v_flex()
                    .min_w_0()
                    .rounded(px(12.))
                    .overflow_hidden()
                    .bg(p.card)
                    .children(rows),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::sorted;
    use crate::store::ProviderAuth;

    #[gpui_kit::test]
    fn a_skipped_server_gets_a_sign_in_notice(cx: &mut gpui_kit::TestAppContext) {
        use super::super::{Page, QmtApp};
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;
        use serde_json::{Value, json};
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        store.session = Some(crate::session::SessionView::new(
            "test-agent",
            "s",
            "/w",
            "S",
        ));
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        let store = cx.update(|cx| app.read(cx).store.clone());
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| app.go(Page::Session, cx));
            store.update(cx, |s, cx| {
                s.notify_for_test(
                    "test-agent",
                    "_querymt/session/mcpAuthRequired",
                    json!({ "version": 1, "session_id": "s",
                            "server_name": "linear", "provider_id": "mcp:linear" }),
                    cx,
                )
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(
            vcx.debug_bounds("mcp-notice").is_some(),
            "notice above the composer"
        );
        let sign_in = vcx
            .debug_bounds("mcp-notice-signin-linear")
            .expect("Sign in");
        vcx.simulate_click(sign_in.center(), gpui_kit::Modifiers::none());
        let sent: Vec<Value> = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|r| serde_json::from_str(&r).unwrap())
            .collect();
        let start = sent
            .iter()
            .find(|r| r["method"] == "_querymt/auth/start")
            .expect("sign-in started");
        assert_eq!(start["params"], json!({ "provider": "mcp:linear" }));
        // Once signed in, the notice offers a new session instead.
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.runtime_mut_for_test("test-agent").mcp_servers =
                    vec![server("linear", "connected")];
                assert!(s.mcp_signed_in("test-agent", "mcp:linear"));
                s.dismiss_skipped_mcp("linear", cx);
                assert!(s.skipped_mcp().is_empty(), "dismissed");
            })
        });
    }

    #[gpui_kit::test]
    fn the_connectors_tab_signs_in_to_a_server(cx: &mut gpui_kit::TestAppContext) {
        use super::super::settings::SettingsTab;
        use super::super::{Page, QmtApp};
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;
        use serde_json::{Value, json};
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.mcp_servers = vec![
            server("linear", "not_authenticated"),
            server("notion", "connected"),
        ];
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                assert!(app.has_connectors(cx), "the tab shows for MCP servers");
                app.go(Page::Settings, cx);
                app.settings_tab = SettingsTab::Connectors;
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(
            vcx.debug_bounds("mcp-signin-notion").is_none(),
            "signed in: Sign out instead"
        );
        let sign_in = vcx.debug_bounds("mcp-signin-linear").expect("Sign in");
        vcx.simulate_click(sign_in.center(), gpui_kit::Modifiers::none());
        // Opening Settings refreshes the sign-in status first.
        let sent: Vec<Value> = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|r| serde_json::from_str(&r).unwrap())
            .collect();
        let start = sent
            .iter()
            .find(|r| r["method"] == "_querymt/auth/start")
            .expect("sign-in started");
        assert_eq!(start["params"], json!({ "provider": "mcp:linear" }));
    }

    fn server(id: &str, status: &str) -> ProviderAuth {
        ProviderAuth {
            provider: format!("mcp:{id}"),
            display_name: format!("mcp:{id}"),
            oauth_status: Some(status.into()),
            supports_oauth: true,
            ..Default::default()
        }
    }

    #[test]
    fn servers_to_sign_in_to_come_first() {
        let order: Vec<String> = sorted(vec![
            server("zeta", "connected"),
            server("Beta", "not_authenticated"),
            server("alpha", "connected"),
            server("gamma", "expired"),
        ])
        .iter()
        .map(|s| s.mcp_server().to_string())
        .collect();
        assert_eq!(order, ["Beta", "gamma", "alpha", "zeta"]);
    }
}
