//! "Fork into a new session" from a reply (desktop `SessionForkDialog`):
//! confirm what the fork keeps, then create it and open it.

use gpui_kit::assets::IconName;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agents::modal_scrim;
use super::{QmtApp, kit};
use crate::session::ForkTarget;
use crate::theme::palette;

/// The fork confirmation: what the fork keeps, and its progress.
pub(super) struct ForkDialog {
    pub(super) target: ForkTarget,
    /// Title of the source session, for the description.
    source: String,
    pub(super) pending: bool,
    pub(super) error: Option<String>,
}

impl QmtApp {
    /// Ask before forking the open session from the turn closing at item
    /// `end` (desktop `openForkDialog`).
    pub(super) fn request_fork(&mut self, end: usize, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if !store.session_supports_fork() || store.fork_pending() {
            return;
        }
        let Some(view) = store.session.as_ref() else {
            return;
        };
        if view.loading || view.running() || view.undo_pending {
            return;
        }
        let Some(target) = view.fork_target(end) else {
            return;
        };
        self.fork = Some(ForkDialog {
            target,
            source: view.title.clone(),
            pending: false,
            error: None,
        });
        cx.notify();
    }

    /// Create the fork; the store opens it, then the dialog closes. A
    /// failure stays in the dialog.
    pub(super) fn confirm_fork(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.fork.as_mut().filter(|d| !d.pending) else {
            return;
        };
        let message_id = dialog.target.message_id.clone();
        let Some(task) = self
            .store
            .update(cx, |s, cx| s.fork_session_at(message_id, cx))
        else {
            dialog.error = Some("The session can't be forked right now.".into());
            cx.notify();
            return;
        };
        dialog.pending = true;
        dialog.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => this.fork = None,
                    Err(err) => {
                        if let Some(dialog) = this.fork.as_mut() {
                            dialog.pending = false;
                            dialog.error = Some(err);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Escape / scrim: close unless the fork is being created.
    pub(super) fn close_fork_dialog(&mut self) -> bool {
        match &self.fork {
            Some(d) if d.pending => true,
            Some(_) => {
                self.fork = None;
                true
            }
            None => false,
        }
    }

    pub(super) fn render_fork_dialog(
        &self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.fork.as_ref()?;
        let p = *palette(cx);
        let pending = dialog.pending;
        let source = if dialog.source.trim().is_empty() {
            "this session"
        } else {
            dialog.source.as_str()
        };
        let section = |label: &'static str, text: String| {
            v_flex()
                .gap_1()
                .child(div().text_sm().font_medium().child(label))
                .child(
                    div()
                        .id(label)
                        .max_h(px(140.))
                        .overflow_y_scroll()
                        .px_3()
                        .py_2()
                        .rounded(px(10.))
                        .bg(p.chip)
                        .text_sm()
                        .text_color(p.muted)
                        .whitespace_normal()
                        .child(text),
                )
        };
        let target = &dialog.target;
        let prompt = if target.prompt.is_empty() {
            "Selected conversation boundary".to_string()
        } else {
            clip_text(&target.prompt)
        };
        let card = self
            .modal_card(&p)
            .w(px(520.))
            .child(div().text_lg().font_semibold().child("Create a new fork?"))
            .child(div().text_sm().text_color(p.muted).child(format!(
                "A new independent session will branch from {source}. \
                 The source and current workspace stay unchanged."
            )))
            .child(section("Prompt", prompt))
            .when(
                target.includes_response && !target.response.is_empty(),
                |el| el.child(section("Included response", clip_text(&target.response))),
            )
            .when_some(dialog.error.clone(), |el, error| {
                el.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(p.danger)
                        .child(kit::icon(IconName::TriangleAlert, 13., p.danger))
                        .child(div().flex_1().min_w_0().child(error)),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        kit::flat_btn("fork-cancel", !pending, true, &p)
                            .child("Cancel")
                            .when(!pending, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| {
                                    this.fork = None;
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        kit::accent_btn("fork-ok", !pending, true, &p)
                            .when(pending, |el| {
                                el.child(kit::spinner("fork-spin", 13., p.on_accent))
                            })
                            .child(if pending {
                                "Creating fork…"
                            } else {
                                "Create and open fork"
                            })
                            .when(!pending, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.confirm_fork(cx)))
                            }),
                    ),
            );
        Some(self.centered(modal_scrim("scrim-fork", radii, &p), card, cx))
    }
}

/// Long prompts and replies: the first part is enough to recognise them.
fn clip_text(text: &str) -> String {
    const MAX: usize = 1200;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::{Page, QmtApp};
    use crate::session::SessionView;

    struct Harness {
        handle: gpui_kit::AnyWindowHandle,
        app: gpui_kit::Entity<QmtApp>,
        rpc: std::rc::Rc<crate::rpc::RpcClient>,
        requests: async_channel::Receiver<String>,
    }

    /// The open session `s` with turns u1 → r-u1 and u2 → r-u2, on an agent
    /// that can fork.
    fn harness(cx: &mut gpui_kit::TestAppContext) -> Harness {
        use gpui_kit::AppContext as _;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut view = SessionView::new("test-agent", "s", "/w", "Source");
        for id in ["u1", "u2"] {
            view.apply(
                &json!({ "sessionUpdate": "user_message_chunk", "messageId": id,
                "content": { "type": "text", "text": format!("prompt {id}") } }),
            );
            view.apply(
                &json!({ "sessionUpdate": "agent_message_chunk", "messageId": format!("r-{id}"),
                "content": { "type": "text", "text": format!("reply {id}") } }),
            );
        }
        view.settle();
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        store.runtime_mut_for_test("test-agent").can_fork_sessions = true;
        store.session = Some(view);
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
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

    fn fork_request(h: &Harness, cx: &mut gpui_kit::TestAppContext) -> Value {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.request_fork(1, cx);
                let dialog = app.fork.as_ref().expect("fork dialog");
                assert_eq!(dialog.target.message_id, "r-u1");
                assert_eq!(dialog.target.prompt, "prompt u1");
            });
            window.render_frame(cx);
            h.app.update(cx, |app, cx| app.confirm_fork(cx));
            window.render_frame(cx);
            // Creating: the dialog stays open, and Escape keeps it.
            h.app.update(cx, |app, _| {
                assert!(app.fork.as_ref().is_some_and(|d| d.pending));
                assert!(app.close_fork_dialog() && app.fork.is_some());
            });
        })
        .unwrap();
        cx.run_until_parked();
        serde_json::from_str(&h.requests.try_recv().unwrap()).unwrap()
    }

    #[gpui_kit::test]
    fn forking_a_reply_opens_the_fork(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        let request = fork_request(&h, cx);
        assert_eq!(request["method"], "session/fork");
        assert_eq!(request["params"]["sessionId"], "s");
        assert_eq!(request["params"]["cwd"], "/w");
        assert_eq!(request["params"]["_meta"]["querymt"]["message_id"], "r-u1");
        h.rpc
            .handle(&json!({ "id": request["id"], "result": { "sessionId": "f1" } }).to_string());
        cx.run_until_parked();
        cx.update(|cx| {
            let app = h.app.read(cx);
            assert!(app.fork.is_none());
            let store = app.store.read(cx);
            let view = store.session.as_ref().unwrap();
            assert_eq!(
                (view.session_id.as_str(), view.title.as_str()),
                ("f1", "Fork of Source")
            );
            let fork = store.session_summary("test-agent", "f1").unwrap();
            assert_eq!(fork.parent.as_deref(), Some("s"));
            assert_eq!(fork.fork_origin.as_deref(), Some("user"));
            assert!(!store.fork_pending());
        });
    }

    #[gpui_kit::test]
    fn a_failed_fork_stays_in_the_dialog(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        let request = fork_request(&h, cx);
        h.rpc.handle(
            &json!({ "id": request["id"], "error": { "code": -32603, "message": "boom" } })
                .to_string(),
        );
        cx.run_until_parked();
        cx.update(|cx| {
            let app = h.app.read(cx);
            let dialog = app.fork.as_ref().expect("dialog stays open");
            assert!(!dialog.pending);
            assert!(dialog.error.as_deref().unwrap().contains("boom"));
            assert_eq!(app.store.read(cx).session.as_ref().unwrap().session_id, "s");
        });
    }
}
