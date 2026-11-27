//! "New worktree session" and "Hand off worktree" from the session menu
//! (desktop `WorktreeSessionDialog`).

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Disableable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agents::modal_scrim;
use super::{QmtApp, kit};
use crate::store::worktree::{WorktreeAction, WorktreeOptions};
use crate::theme::{Palette, palette};

pub(super) struct WorktreeDialog {
    pub(super) action: WorktreeAction,
    source: String,
    name: Entity<InputState>,
    base_ref: Entity<InputState>,
    pub(super) include_local_changes: bool,
    pub(super) summary: bool,
    /// The agent can seed a handoff with a summary.
    summary_supported: bool,
    pub(super) pending: bool,
    pub(super) error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl QmtApp {
    /// Open the dialog for `action` on the open session; each opening
    /// starts from clean defaults (summary off).
    pub(super) fn open_worktree_dialog(
        &mut self,
        action: WorktreeAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let Some(skey) = self.main_key(cx) else {
            return;
        };
        let allowed = match action {
            WorktreeAction::Create => store.can_create_worktree(&skey, cx),
            WorktreeAction::Handoff => store.can_handoff_worktree(&skey, cx),
        };
        if !allowed {
            return;
        }
        let source = self
            .main_view(cx)
            .map(|v| v.title.clone())
            .unwrap_or_default();
        let summary_supported = store.handoff_summary_supported(&skey, cx);
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("feature-x", window, cx);
        let base_ref = input("main", window, cx);
        let mut subscriptions = Vec::new();
        for field in [&name, &base_ref] {
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::PressEnter { .. } => this.confirm_worktree(cx),
                    InputEvent::Change => {
                        if let Some(d) = this.worktree.as_mut() {
                            d.error = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            ));
        }
        name.update(cx, |s, cx| s.focus(window, cx));
        self.pane.update(cx, |pane, _| pane.menu_open = false);
        self.worktree = Some(WorktreeDialog {
            action,
            source,
            name,
            base_ref,
            include_local_changes: false,
            summary: false,
            summary_supported,
            pending: false,
            error: None,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn worktree_options(&self, cx: &App) -> Option<WorktreeOptions> {
        let d = self.worktree.as_ref()?;
        Some(WorktreeOptions {
            name: d.name.read(cx).value().to_string(),
            base_ref: d.base_ref.read(cx).value().to_string(),
            include_local_changes: d.include_local_changes,
            summary: d.summary_supported && d.summary,
        })
    }

    /// Run the action; the store opens the new session, then the dialog
    /// closes. A failure stays in the dialog.
    pub(super) fn confirm_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(options) = self.worktree_options(cx) else {
            return;
        };
        let Some(dialog) = self.worktree.as_mut().filter(|d| !d.pending) else {
            return;
        };
        let action = dialog.action;
        if options.problem(action).is_some() {
            return;
        }
        let Some(skey) = self.main_key(cx) else {
            return;
        };
        let Some(task) = self.store.update(cx, |s, cx| {
            s.start_worktree_action(&skey, action, options, cx)
        }) else {
            if let Some(d) = self.worktree.as_mut() {
                d.error = Some("This can't be done for the session right now.".into());
            }
            cx.notify();
            return;
        };
        if let Some(d) = self.worktree.as_mut() {
            d.pending = true;
            d.error = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => this.worktree = None,
                    Err(err) => {
                        if let Some(d) = this.worktree.as_mut() {
                            d.pending = false;
                            d.error = Some(err);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Escape / scrim: close unless the action is running.
    pub(super) fn close_worktree_dialog(&mut self) -> bool {
        match &self.worktree {
            Some(d) if d.pending => true,
            Some(_) => {
                self.worktree = None;
                true
            }
            None => false,
        }
    }

    pub(super) fn render_worktree_dialog(
        &self,
        radii: Corners<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let d = self.worktree.as_ref()?;
        let p = *palette(cx);
        let pending = d.pending;
        let create = d.action == WorktreeAction::Create;
        let problem = self.worktree_options(cx).and_then(|o| o.problem(d.action));
        let (title, description) = if create {
            let source = if d.source.trim().is_empty() {
                "The source session"
            } else {
                d.source.as_str()
            };
            (
                "New worktree session",
                format!(
                    "Start an isolated session in a new Git worktree. {source} stays unchanged."
                ),
            )
        } else {
            let source = if d.source.trim().is_empty() {
                "This session"
            } else {
                d.source.as_str()
            };
            (
                "Hand off worktree",
                format!(
                    "Start a fresh conversation in the same checkout. {source} stays readable \
                     but can no longer run."
                ),
            )
        };
        let field = |label: &'static str,
                     input: &Entity<InputState>,
                     help: Option<&'static str>,
                     disabled: bool| {
            let focused = input.read(cx).focus_handle(cx).is_focused(window);
            v_flex()
                .gap_1p5()
                .child(div().text_sm().font_medium().child(label))
                .child(
                    kit::field_shell(focused, &p).h(px(34.)).child(
                        Input::new(input)
                            .appearance(false)
                            .aria_label(label)
                            .disabled(pending || disabled)
                            .flex_1(),
                    ),
                )
                .children(help.map(|h| div().text_xs().text_color(p.muted).child(h)))
        };
        let card = self
            .modal_card(&p)
            .w(px(480.))
            .child(div().text_lg().font_semibold().child(title))
            .child(div().text_sm().text_color(p.muted).child(description))
            .child(field("Name (optional)", &d.name, None, false))
            .when(create, |el| {
                el.child(field(
                    "Base ref (optional)",
                    &d.base_ref,
                    Some(if d.include_local_changes {
                        "Not used with local changes: the worktree starts from your checkout."
                    } else {
                        "Branch, tag or commit to start from."
                    }),
                    d.include_local_changes,
                ))
                .child(switch_row(
                    "worktree-local-changes",
                    "Include local changes",
                    "Copy uncommitted work into the worktree. Exclusive with a base ref.",
                    d.include_local_changes,
                    pending,
                    &p,
                    cx.listener(|this, on: &bool, _, cx| {
                        if let Some(d) = this.worktree.as_mut() {
                            d.include_local_changes = *on;
                            d.error = None;
                        }
                        cx.notify();
                    }),
                ))
            })
            .when(!create && d.summary_supported, |el| {
                el.child(switch_row(
                    "worktree-summary",
                    "Start with a summary",
                    "Seeds the new conversation with a summary of this one. This can take a few \
                     seconds.",
                    d.summary,
                    pending,
                    &p,
                    cx.listener(|this, on: &bool, _, cx| {
                        if let Some(d) = this.worktree.as_mut() {
                            d.summary = *on;
                        }
                        cx.notify();
                    }),
                ))
            })
            .children(
                d.error
                    .clone()
                    .or_else(|| problem.map(str::to_owned))
                    .map(|error| {
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(p.danger)
                            .child(kit::icon(IconName::TriangleAlert, 13., p.danger))
                            .child(div().flex_1().min_w_0().child(error))
                    }),
            )
            .child({
                let ready = !pending && problem.is_none();
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        kit::flat_btn("worktree-cancel", !pending, true, &p)
                            .child("Cancel")
                            .when(!pending, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| {
                                    this.worktree = None;
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        kit::accent_btn("worktree-ok", ready, true, &p)
                            .when(pending, |el| {
                                el.child(kit::spinner("worktree-spin", 13., p.on_accent))
                            })
                            .child(match (create, pending) {
                                (true, false) => "Create and open",
                                (true, true) => "Creating…",
                                (false, false) => "Hand off and open",
                                (false, true) => "Handing off…",
                            })
                            .when(ready, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.confirm_worktree(cx)))
                            }),
                    )
            });
        Some(self.centered(modal_scrim("scrim-worktree", radii, &p), card, cx))
    }
}

/// A labelled switch with help text, as in Settings.
fn switch_row(
    id: &'static str,
    label: &'static str,
    help: &'static str,
    checked: bool,
    disabled: bool,
    p: &Palette,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Div {
    h_flex()
        .gap_4()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_medium().child(label))
                .child(div().text_xs().text_color(p.muted).child(help)),
        )
        .child(
            kit::switch(id)
                .checked(checked)
                .disabled(disabled)
                .on_click(on_click),
        )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::{Page, QmtApp};
    use crate::session::SessionView;
    use crate::store::launch::WorktreeSupport;
    use crate::store::worktree::WorktreeAction;
    use crate::store::{SessionSummary, Worktree};

    struct Harness {
        handle: gpui_kit::AnyWindowHandle,
        app: gpui_kit::Entity<QmtApp>,
        rpc: std::rc::Rc<crate::rpc::RpcClient>,
        requests: async_channel::Receiver<String>,
    }

    /// The open session `s` ("Source", in `/repo`) owns a worktree of
    /// `/repo` on an agent with managed worktrees and summary handoffs.
    fn harness(cx: &mut gpui_kit::TestAppContext) -> Harness {
        use gpui_kit::AppContext as _;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.worktrees = Some(WorktreeSupport {
            handoff: true,
            handoff_summary: true,
            source_session: true,
        });
        rt.sessions = vec![SessionSummary {
            agent_id: "test-agent".into(),
            id: "s".into(),
            cwd: "/wt/s".into(),
            title: "Source".into(),
            phase: "idle".into(),
            root: true,
            open: true,
            worktree: Some(Worktree {
                branch: Some("qmt/s".into()),
                checkout_path: Some("/wt/s".into()),
                source_cwd: Some("/repo".into()),
                lifecycle: Some("ready".into()),
                owner_session_id: Some("s".into()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let main_view = SessionView::new("test-agent", "s", "/wt/s", "Source");
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|cx| {
                let mut store = store;
                store.install_main_for_test(main_view, cx);
                store
            });
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        Harness {
            handle,
            app,
            rpc,
            requests,
        }
    }

    /// Open the dialog for `action`, fill it with `fill`, confirm, and
    /// return the request sent.
    fn submit(
        h: &Harness,
        action: WorktreeAction,
        fill: impl FnOnce(&mut QmtApp, &mut gpui_kit::Window, &mut gpui_kit::Context<QmtApp>),
        cx: &mut gpui_kit::TestAppContext,
    ) -> Value {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.open_worktree_dialog(action, window, cx);
                assert!(app.worktree.is_some());
                fill(app, window, cx);
            });
            window.render_frame(cx);
            h.app.update(cx, |app, cx| app.confirm_worktree(cx));
            window.render_frame(cx);
            h.app.update(cx, |app, _| {
                assert!(app.worktree.as_ref().is_some_and(|d| d.pending));
                // Running: Escape keeps the dialog.
                assert!(app.close_worktree_dialog() && app.worktree.is_some());
            });
        })
        .unwrap();
        cx.run_until_parked();
        serde_json::from_str(&h.requests.try_recv().unwrap()).unwrap()
    }

    fn set(
        input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
        text: &str,
        window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::Context<QmtApp>,
    ) {
        input.update(cx, |s, cx| s.set_value(text.to_string(), window, cx));
    }

    #[gpui_kit::test]
    fn a_new_worktree_session_opens(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        let request = submit(
            &h,
            WorktreeAction::Create,
            |app, window, cx| {
                let d = app.worktree.as_ref().unwrap();
                let (name, base) = (d.name.clone(), d.base_ref.clone());
                set(&name, "feat", window, cx);
                set(&base, "main", window, cx);
            },
            cx,
        );
        assert_eq!(request["method"], "_querymt/worktree/create");
        assert_eq!(
            request["params"],
            json!({ "cwd": "/repo", "source_session_id": "s", "name": "feat", "base_ref": "main" })
        );
        h.rpc.handle(
            &json!({ "id": request["id"], "result": {
                "session_id": "w1", "session_cwd": "/wt/w1", "checkout_path": "/wt/w1",
                "branch": "qmt/feat", "workspace_id": "ws1", "baseline_mode": "commit"
            } })
            .to_string(),
        );
        cx.run_until_parked();
        cx.update(|cx| {
            let app = h.app.read(cx);
            assert!(app.worktree.is_none());
            let store = app.store.read(cx);
            let view = app.main_view(cx).unwrap();
            assert_eq!(
                (
                    view.session_id.as_str(),
                    view.cwd.as_str(),
                    view.title.as_str()
                ),
                ("w1", "/wt/w1", "feat")
            );
            let child = store.session_summary("test-agent", "w1").unwrap();
            assert_eq!(child.parent.as_deref(), Some("s"));
            assert_eq!(child.fork_origin.as_deref(), Some("worktree"));
            let w = child.worktree.as_ref().unwrap();
            assert_eq!(w.branch.as_deref(), Some("qmt/feat"));
            assert_eq!(w.source_cwd.as_deref(), Some("/repo"));
            // The source keeps its worktree.
            let source = store.session_summary("test-agent", "s").unwrap();
            assert!(source.worktree.as_ref().unwrap().handed_off_to.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_handoff_opens_the_successor(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        let request = submit(
            &h,
            WorktreeAction::Handoff,
            |app, _, _| app.worktree.as_mut().unwrap().summary = true,
            cx,
        );
        assert_eq!(request["method"], "_querymt/worktree/handoff");
        assert_eq!(
            request["params"],
            json!({ "session_id": "s", "summary": true })
        );
        h.rpc.handle(
            &json!({ "id": request["id"], "result": {
                "session_id": "h1", "previous_session_id": "s", "session_cwd": "/wt/s",
                "checkout_path": "/wt/s", "branch": "qmt/s", "workspace_id": "ws",
                "summary_seeded": true
            } })
            .to_string(),
        );
        cx.run_until_parked();
        cx.update(|cx| {
            let app = h.app.read(cx);
            assert!(app.worktree.is_none());
            let store = app.store.read(cx);
            let view = app.main_view(cx).unwrap();
            assert_eq!(
                (view.session_id.as_str(), view.title.as_str()),
                ("h1", "Source")
            );
            let source = store.session_summary("test-agent", "s").unwrap();
            let w = source.worktree.as_ref().unwrap();
            assert_eq!(w.handed_off_to.as_deref(), Some("h1"));
        });
    }

    #[gpui_kit::test]
    fn a_refused_handoff_explains_itself(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        let request = submit(&h, WorktreeAction::Handoff, |_, _, _| {}, cx);
        // Clean by default: no summary unless asked for.
        assert_eq!(request["params"], json!({ "session_id": "s" }));
        h.rpc.handle(
            &json!({ "id": request["id"], "error": {
                "code": -32600, "message": "Invalid request",
                "data": { "code": "owner_active", "message": "busy" }
            } })
            .to_string(),
        );
        cx.run_until_parked();
        cx.update(|cx| {
            let app = h.app.read(cx);
            let d = app.worktree.as_ref().expect("dialog stays open");
            assert!(!d.pending);
            assert!(
                d.error
                    .as_deref()
                    .unwrap()
                    .starts_with("Stop the running turn")
            );
            let store = app.store.read(cx);
            assert_eq!(app.main_view(cx).unwrap().session_id, "s");
            assert!(
                !store.worktree_pending(&crate::session::open::SessionKey::new("test-agent", "s"))
            );
        });
    }

    #[gpui_kit::test]
    fn session_details_show_the_worktree_and_copy_its_checkout(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;
        let h = harness(cx);
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                // Listed only under its parent, like a worktree child.
                let store = app.store.clone();
                store.update(cx, |s, _| {
                    let rt = s.runtime_mut_for_test("test-agent");
                    let child = rt.sessions.remove(0);
                    rt.children.entry("p".into()).or_default().sessions = vec![child];
                    assert!(s.session_summary("test-agent", "s").is_some());
                });
                app.pane.update(cx, |pane, _| pane.details_open = true);
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(h.handle, cx);
        let checkout = vcx.debug_bounds("details-checkout").expect("checkout row");
        vcx.simulate_click(checkout.center(), gpui_kit::Modifiers::none());
        let copied = cx.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(copied.as_deref(), Some("/wt/s"));
    }

    #[gpui_kit::test]
    fn a_worktree_child_listed_under_its_parent_can_hand_off(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        cx.update(|cx| {
            let store = h.app.read(cx).store.clone();
            store.update(cx, |s, cx| {
                // After a restart the child is only in its parent's list.
                let rt = s.runtime_mut_for_test("test-agent");
                let child = rt.sessions.remove(0);
                rt.children.entry("p".into()).or_default().sessions = vec![child];
                assert!(
                    s.can_create_worktree(
                        &crate::session::open::SessionKey::new("test-agent", "s"),
                        cx
                    ) && s.can_handoff_worktree(
                        &crate::session::open::SessionKey::new("test-agent", "s"),
                        cx
                    )
                );
            });
        });
    }

    #[gpui_kit::test]
    fn a_read_only_session_offers_no_worktree_actions(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;
        let h = harness(cx);
        cx.update_window(h.handle, |_, window, cx| {
            let store = h.app.read(cx).store.clone();
            let session = h.app.read(cx).pane.read(cx).session().cloned().unwrap();
            let key = crate::session::open::SessionKey::new("test-agent", "s");
            store.update(cx, |s, cx| {
                assert!(s.can_create_worktree(&key, cx) && s.can_handoff_worktree(&key, cx));
            });
            session.update(cx, |o, _| {
                o.view.access = crate::session::SessionAccess::parse(&json!({
                    "_meta": { "querymt": { "sessionAccess": {
                        "mode": "read_only", "reason": "owner_transferred", "ownerSessionId": "h1"
                    } } }
                }));
            });
            store.update(cx, |s, cx| {
                assert!(!s.can_create_worktree(&key, cx) && !s.can_handoff_worktree(&key, cx));
                assert!(!s.session_supports_fork(&key, cx));
            });
            assert!(!session.read(cx).supports_undo());
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.open_worktree_dialog(WorktreeAction::Create, window, cx);
                assert!(app.worktree.is_none());
            });
            // The notice replaces the composer.
            window.render_frame(cx);
        })
        .unwrap();
    }
}
