//! The quick panel: summoning, dismissing, quick chats, the session picker
//! and the hand-off to the main window (spec `quick-panel`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, VisualTestContext};
use serde_json::{Value, json};

use super::panel::{picker_height, picker_rows};
use super::{QuickHost, QuickPanel, QuickState, RELEASE_AFTER};
use crate::rpc::RpcClient;
use crate::session::open::{OpenSession, SessionKey, SessionRef};
use crate::session::{ListedPlan, PlanEntry, SessionView};
use crate::store::{AgentsStore, ConnState, PickerLayout, Prefs, SessionSummary};
use crate::ui::QmtApp;

struct Rig {
    store: Entity<AgentsStore>,
    host: Entity<QuickHost>,
    main: AnyWindowHandle,
    app: Entity<QmtApp>,
    rpc: Rc<RpcClient>,
    requests: async_channel::Receiver<String>,
}

fn rig(cx: &mut TestAppContext) -> Rig {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
    });
    let mut store = crate::store::automations::schedule_ui_fixture();
    store.prefs.cwd = "/w".into();
    let (rpc, requests) = store.rpc_for_test("test-agent");
    let (store, host, main, app) = cx.update(|cx| {
        let store = cx.new(|_| store);
        let (main, app) = crate::ui::open_main_window(&store, cx).unwrap();
        let host = QuickHost::install(store.clone(), Some((main, app.clone())), cx);
        (store, host, main, app)
    });
    let rig = Rig {
        store,
        host,
        main,
        app,
        rpc,
        requests,
    };
    rig.sent(cx);
    rig
}

impl Rig {
    /// Requests sent since the last call.
    fn sent(&self, cx: &mut TestAppContext) -> Vec<Value> {
        cx.run_until_parked();
        std::iter::from_fn(|| self.requests.try_recv().ok())
            .map(|text| serde_json::from_str(&text).unwrap())
            .collect()
    }

    fn reply(&self, cx: &mut TestAppContext, request: &Value, result: Value) {
        self.rpc
            .handle(&json!({ "id": request["id"], "result": result }).to_string());
        cx.run_until_parked();
    }

    /// Answer what the agent is asked, except prompts, which are returned.
    fn pump(&self, cx: &mut TestAppContext) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let batch = self.sent(cx);
            if batch.is_empty() {
                return seen;
            }
            for request in batch {
                match request["method"].as_str() {
                    Some("session/new") => {
                        self.reply(cx, &request, json!({ "sessionId": "q1" }));
                    }
                    Some("session/prompt") => {}
                    _ => self.reply(cx, &request, json!({})),
                }
                seen.push(request);
            }
        }
    }

    fn show(&self, cx: &mut TestAppContext) {
        cx.update(|cx| self.host.update(cx, |h, cx| h.show(cx)));
        cx.run_until_parked();
    }

    fn hide(&self, cx: &mut TestAppContext) {
        cx.update(|cx| self.host.update(cx, |h, cx| h.hide(cx)));
        cx.run_until_parked();
    }

    fn panel(&self, cx: &mut TestAppContext) -> Option<(AnyWindowHandle, Entity<QuickPanel>)> {
        self.host.read_with(cx, |h, _| h.panel_for_test())
    }

    fn open_panel(&self, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<QuickPanel>) {
        self.show(cx);
        self.panel(cx).expect("the panel is open")
    }

    fn summaries(&self, cx: &mut TestAppContext, sessions: Vec<SessionSummary>) {
        cx.update(|cx| {
            self.store.update(cx, |s, _| {
                s.runtime_mut_for_test("test-agent").sessions = sessions;
            })
        });
    }

    fn main_key(&self, cx: &mut TestAppContext) -> Option<SessionKey> {
        self.app.read_with(cx, |app, cx| app.main_key(cx))
    }
}

fn summary(id: &str, title: &str, phase: &str, updated: &str) -> SessionSummary {
    SessionSummary {
        agent_id: "test-agent".into(),
        id: id.into(),
        cwd: format!("/work/{id}"),
        title: title.into(),
        updated_at: updated.into(),
        phase: phase.into(),
        root: true,
        open: true,
        ..Default::default()
    }
}

fn pane_key(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> Option<String> {
    panel.read_with(cx, |p, cx| {
        p.pane_for_test()
            .read(cx)
            .key(cx)
            .map(|k| k.session.to_string())
    })
}

fn pane_entity(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> Option<Entity<OpenSession>> {
    panel.read_with(cx, |p, cx| p.pane_for_test().read(cx).session().cloned())
}

fn draft(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> String {
    panel.read_with(cx, |p, cx| p.pane_for_test().read(cx).draft(cx))
}

fn type_draft(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
    text: &str,
) {
    cx.update_window(window, |_, window, cx| {
        let pane = panel.read(cx).pane_for_test().clone();
        pane.update(cx, |pane, cx| pane.set_draft(text, window, cx));
    })
    .unwrap();
}

fn pick(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
    summary: &SessionSummary,
) {
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.pick(summary, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
}

fn closed(cx: &mut TestAppContext, rig: &Rig) -> bool {
    rig.panel(cx).is_none()
}

fn state_has_session(cx: &mut TestAppContext, rig: &Rig) -> bool {
    rig.host.read_with(cx, |h, _| h.state().session.is_some())
}

// ---- state and the release timer ------------------------------------------

#[gpui_kit::test]
fn release_timer_drops_quick_session(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(cx, vec![summary("q", "Quick one", "idle", "2")]);
    let (window, panel) = rig.open_panel(cx);
    pick(cx, window, &panel, &summary("q", "Quick one", "idle", "2"));
    type_draft(cx, window, &panel, "half typed");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.dismiss(window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    assert!(closed(cx, &rig));
    assert!(state_has_session(cx, &rig));

    // Just before the time is up the conversation is still held.
    cx.executor()
        .advance_clock(RELEASE_AFTER - Duration::from_secs(1));
    cx.run_until_parked();
    assert!(state_has_session(cx, &rig));

    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(!state_has_session(cx, &rig));
    rig.host
        .read_with(cx, |h, _| assert_eq!(h.state().draft, ""));

    // The next opening starts on a new, empty quick chat.
    let (_, panel) = rig.open_panel(cx);
    assert_eq!(pane_key(cx, &panel), None);
    assert_eq!(draft(cx, &panel), "");
}

#[gpui_kit::test]
fn reopening_shortly_after_returns_to_the_session(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let q = summary("q", "Quick one", "idle", "2");
    rig.summaries(cx, vec![q.clone()]);
    let (window, panel) = rig.open_panel(cx);
    pick(cx, window, &panel, &q);
    let session = pane_entity(cx, &panel).expect("session shown");
    type_draft(cx, window, &panel, "half typed");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.dismiss(window, cx))
    })
    .unwrap();
    cx.run_until_parked();

    cx.executor().advance_clock(Duration::from_secs(60));
    let (_, panel) = rig.open_panel(cx);
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("q"));
    assert_eq!(
        pane_entity(cx, &panel).map(|s| s.entity_id()),
        Some(session.entity_id())
    );
    assert_eq!(draft(cx, &panel), "half typed");
}

#[test]
fn state_release_needs_the_full_time() {
    let mut state = QuickState::default();
    let start = std::time::Instant::now();
    state.hide(None, "draft".into(), start);
    assert!(!state.release_if_due(start + RELEASE_AFTER - Duration::from_millis(1)));
    assert_eq!(state.draft, "draft");
    assert!(state.release_if_due(start + RELEASE_AFTER));
    assert_eq!(state.draft, "");
    assert!(state.hidden_at.is_none());
}

// ---- summon and dismiss ---------------------------------------------------

#[gpui_kit::test]
fn esc_closes_and_keeps_draft(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    type_draft(cx, window, &panel, "half typed");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();

    assert!(closed(cx, &rig), "Esc closes the panel");
    rig.host.read_with(cx, |h, _| {
        assert_eq!(h.state().draft, "half typed");
        assert!(h.state().hidden_at.is_some());
    });
    let (_, panel) = rig.open_panel(cx);
    assert_eq!(draft(cx, &panel), "half typed");
}

// The window's root must not paint the app theme's opaque background: it
// would cover the material and every layer under GPUI's view (found with the
// effect probe of task 7.14).
#[gpui_kit::test]
fn the_panel_window_root_paints_no_background(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, _) = rig.open_panel(cx);
    let background = cx
        .update_window(window, |_, window, cx| {
            gpui_kit::base::Root::update(window, cx, |root, _, _| {
                gpui_kit::Styled::style(root).background.clone()
            })
        })
        .unwrap();
    let Some(gpui_kit::Fill::Color(background)) = background else {
        panic!("the panel's root sets its own background, got {background:?}");
    };
    assert_eq!(
        background,
        gpui_kit::Background::from(gpui_kit::transparent_black()),
        "transparent"
    );
}

#[gpui_kit::test]
fn deactivation_closes_panel(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    type_draft(cx, window, &panel, "keep me");
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.deactivate_window();
    cx.run_until_parked();

    assert!(closed(cx, &rig), "losing focus closes the panel");
    rig.host
        .read_with(cx, |h, _| assert_eq!(h.state().draft, "keep me"));
}

#[gpui_kit::test]
fn toggling_twice_closes_the_panel(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.show(cx);
    assert!(rig.panel(cx).is_some());
    cx.update(|cx| rig.host.update(cx, |h, cx| h.toggle(cx)));
    cx.run_until_parked();
    assert!(closed(cx, &rig));
}

// ---- quick chat -----------------------------------------------------------

#[gpui_kit::test]
fn quick_chat_launches_on_primary_agent(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    type_draft(cx, window, &panel, "what is a monad?");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.submit(window, cx))
    })
    .unwrap();
    let sent = rig.pump(cx);

    let new = sent
        .iter()
        .find(|r| r["method"] == "session/new")
        .expect("a session was created");
    assert_eq!(new["params"]["cwd"], "/w");
    // Local placement: no worktree or peer in the request.
    assert!(new["params"].get("_meta").is_none());
    let prompt = sent
        .iter()
        .find(|r| r["method"] == "session/prompt")
        .expect("the prompt was sent");
    assert_eq!(prompt["params"]["sessionId"], "q1");
    assert!(prompt.to_string().contains("what is a monad?"));

    // The session shows in the panel, the draft is gone, the main window is untouched.
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("q1"));
    assert_eq!(draft(cx, &panel), "");
    assert_eq!(rig.main_key(cx), None);
}

#[gpui_kit::test]
fn quick_chat_without_connection_keeps_draft(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| {
        rig.store.update(cx, |s, _| {
            s.runtime_mut_for_test("test-agent").state = ConnState::Idle;
        })
    });
    let (window, panel) = rig.open_panel(cx);
    type_draft(cx, window, &panel, "are you there?");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.submit(window, cx))
    })
    .unwrap();
    let sent = rig.pump(cx);

    assert!(sent.iter().all(|r| r["method"] != "session/new"));
    assert_eq!(draft(cx, &panel), "are you there?");
    assert_eq!(pane_key(cx, &panel), None);
    panel.read_with(cx, |p, _| {
        assert!(
            p.error_for_test()
                .is_some_and(|e| e.contains("not connected")),
            "{:?}",
            p.error_for_test()
        );
    });
}

// ---- picker ---------------------------------------------------------------

#[gpui_kit::test]
fn picker_orders_waiting_then_running(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("newest", "Newest idle", "idle", "2026-05-04"),
            summary("run", "Running", "running", "2026-05-02"),
            summary("old", "Old idle", "idle", "2026-05-01"),
            summary("wait", "Waiting", "running", "2026-05-01"),
        ],
    );
    cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.request_for_test(
                "test-agent",
                1,
                "session/request_permission",
                json!({
                    "sessionId": "wait",
                    "toolCall": { "toolCallId": "t1", "title": "Run ls", "kind": "execute" },
                    "options": [{ "optionId": "o", "name": "Allow", "kind": "allow_once" }],
                }),
                cx,
            )
        })
    });
    let ids = |query: &str, cx: &mut TestAppContext| -> Vec<String> {
        rig.store
            .read_with(cx, |s, cx| picker_rows(s, query, cx))
            .into_iter()
            .map(|r| r.id)
            .collect()
    };
    assert_eq!(ids("", cx), ["wait", "run", "newest", "old"]);
    // Search by title, workspace or agent.
    assert_eq!(ids("old idle", cx), ["old"]);
    assert_eq!(ids("/work/run", cx), ["run"]);
    assert_eq!(ids("test agent", cx).len(), 4);
    assert!(ids("nothing like this", cx).is_empty());
}

#[gpui_kit::test]
fn picking_changes_only_the_panel(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let a = summary("a", "Session A", "running", "2");
    rig.summaries(cx, vec![a.clone(), summary("m", "In main", "idle", "1")]);
    cx.update(|cx| {
        rig.app.update(cx, |app, cx| {
            app.show_session(
                SessionRef {
                    key: SessionKey::new("test-agent", "m"),
                    cwd: "/w".into(),
                    title: "In main".into(),
                },
                cx,
            )
        })
    });
    cx.run_until_parked();
    let (window, panel) = rig.open_panel(cx);
    pick(cx, window, &panel, &a);

    assert_eq!(pane_key(cx, &panel).as_deref(), Some("a"));
    assert_eq!(
        rig.main_key(cx).map(|k| k.session.to_string()).as_deref(),
        Some("m")
    );
}

// ---- open in the main window ----------------------------------------------

#[gpui_kit::test]
fn open_in_main_reuses_entity_without_load(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let a = summary("a", "Session A", "idle", "2");
    rig.summaries(cx, vec![a.clone()]);
    let (window, panel) = rig.open_panel(cx);
    pick(cx, window, &panel, &a);
    let loads = rig.pump(cx);
    assert_eq!(
        loads
            .iter()
            .filter(|r| r["method"] == "session/load")
            .count(),
        1
    );
    let session = pane_entity(cx, &panel).expect("session shown");

    cx.update(|cx| panel.update(cx, |p, cx| p.open_in_main(None, cx)));
    cx.run_until_parked();

    // The main window shows the very same session, the panel is gone, and
    // the agent was not asked to load it again.
    assert_eq!(
        rig.main_key(cx).map(|k| k.session.to_string()).as_deref(),
        Some("a")
    );
    let in_main = rig.app.read_with(cx, |app, cx| {
        app.main_view(cx).map(|v| v.session_id.clone())
    });
    assert_eq!(in_main.as_deref(), Some("a"));
    let main_session = rig
        .app
        .read_with(cx, |app, cx| app.main_session_for_test(cx))
        .expect("main shows a session");
    assert_eq!(main_session.entity_id(), session.entity_id());
    assert!(closed(cx, &rig));
    assert!(!state_has_session(cx, &rig));
    assert!(rig.pump(cx).iter().all(|r| r["method"] != "session/load"));
}

#[gpui_kit::test]
fn open_in_main_reopens_a_closed_main_window(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let a = summary("a", "Session A", "idle", "2");
    rig.summaries(cx, vec![a.clone()]);
    let (window, panel) = rig.open_panel(cx);
    pick(cx, window, &panel, &a);
    let session = pane_entity(cx, &panel).expect("session shown");

    // Close the main window; the panel keeps the app alive.
    cx.update_window(rig.main, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert!(rig.panel(cx).is_some());

    cx.update(|cx| panel.update(cx, |p, cx| p.open_in_main(None, cx)));
    cx.run_until_parked();

    let main = rig
        .host
        .read_with(cx, |h, _| h.main_for_test())
        .expect("main window");
    assert_ne!(main, rig.main, "a new main window was opened");
    let app = rig
        .host
        .read_with(cx, |h, _| h.main_app_for_test())
        .expect("main app");
    let shown = app.read_with(cx, |app, cx| app.main_session_for_test(cx));
    assert_eq!(shown.map(|s| s.entity_id()), Some(session.entity_id()));
    assert!(closed(cx, &rig));
}

#[gpui_kit::test]
fn the_tray_reuses_the_main_window_and_reopens_it_if_closed(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let open_from_tray = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            rig.host.update(cx, |host, cx| {
                host.on_signal(
                    super::Signal::OpenMain {
                        activation_token: Some("test-tray-token".into()),
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();
        rig.host.read_with(cx, |h, _| h.main_for_test()).unwrap()
    };

    assert_eq!(open_from_tray(cx), rig.main);
    cx.update_window(rig.main, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_ne!(open_from_tray(cx), rig.main);
}

#[gpui_kit::test]
fn a_session_view_can_be_shown_without_the_main_window(cx: &mut TestAppContext) {
    // The panel's pane only needs the store; adopting a view shows it.
    let rig = rig(cx);
    let (_, panel) = rig.open_panel(cx);
    let session = cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.adopt_for_test(SessionView::new("test-agent", "z", "/w", "Z"), cx)
        })
    });
    cx.update(|cx| {
        panel.update(cx, |p, cx| {
            p.pane_for_test()
                .clone()
                .update(cx, |pane, cx| pane.show(session, cx))
        })
    });
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("z"));
}

// ---- motion, glow and height (design D13, D14) ------------------------------

use super::edge::{EdgeColours, EdgeError, EdgeLight, EdgeState, MOTION, Show};
use super::panel::{BAR_HEIGHT, MIN_HEIGHT, fitted_height, open_opacity, open_scale};

/// Render the panel's window until its layout and resizes settle.
fn settle(cx: &mut TestAppContext, window: AnyWindowHandle) {
    for _ in 0..12 {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        // A real platform reports a resize back to GPUI, which then lays out
        // at the new size; the test platform only stores it.
        let bounds = cx
            .update_window(window, |_, window, _| window.bounds())
            .unwrap();
        cx.simulate_window_resize(window, bounds.size);
    }
}

/// The height asked of the window. Where the window really is that tall is
/// [`window_height`]: the two must agree once things settle.
fn panel_height(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> f32 {
    panel.read_with(cx, |p, _| p.height_for_test())
}

/// The height the platform window has. The test platform stores a resize but
/// never reports it back to GPUI (`viewport_size` stays put), so this is as far
/// as a test can see: that the request reached the window. That the real macOS
/// window then reports it, which was the bug where the window grew but GPUI
/// kept drawing at the old size, is a manual check.
fn window_height(cx: &mut TestAppContext, window: AnyWindowHandle) -> f32 {
    cx.update_window(window, |_, window, _| {
        f32::from(window.bounds().size.height)
    })
    .unwrap()
}

/// A session of `turns` question and answer pairs, each answer a few lines.
fn long_view(id: &str, turns: usize, phase: &str) -> SessionView {
    let mut view = SessionView::new("test-agent", id, "/w", id);
    for i in 0..turns {
        view.apply(
            &json!({ "sessionUpdate": "user_message_chunk", "messageId": format!("u{i}"),
            "content": { "type": "text", "text": format!("question {i}") } }),
        );
        view.apply(&json!({ "sessionUpdate": "agent_message_chunk", "messageId": format!("a{i}"),
            "content": { "type": "text", "text": "line one\n\nline two\n\nline three\n\nline four" } }));
    }
    view.phase = phase.into();
    view
}

/// Show `view` in the panel's pane.
fn show_view(
    cx: &mut TestAppContext,
    rig: &Rig,
    panel: &Entity<QuickPanel>,
    view: SessionView,
) -> Entity<OpenSession> {
    let session = cx.update(|cx| rig.store.update(cx, |s, cx| s.adopt_for_test(view, cx)));
    cx.update(|cx| {
        panel.update(cx, |p, cx| {
            let session = session.clone();
            p.pane_for_test()
                .clone()
                .update(cx, |pane, cx| pane.show(session, cx))
        })
    });
    session
}

#[test]
fn fitted_height_grows_in_steps_and_stops_at_the_cap() {
    // Overflow is added and rounded up to a step.
    assert_eq!(fitted_height(260., 13., 0., 1000., true), 276.);
    assert_eq!(fitted_height(260., 0., 0., 1000., true), 260.);
    // min(760, 70 % of the display).
    assert_eq!(fitted_height(260., 5000., 0., 1000., true), 700.);
    assert_eq!(fitted_height(260., 5000., 0., 2000., true), 760.);
    // A small display still gets the compact height.
    assert_eq!(fitted_height(260., 5000., 0., 300., true), MIN_HEIGHT);
}

#[test]
fn fitted_height_never_shrinks_with_a_session_but_follows_an_empty_chat() {
    assert_eq!(fitted_height(500., 0., 0., 1000., true), 500.);
    assert_eq!(fitted_height(500., 0., 400., 1000., true), 500.);
    // An empty chat returns to its bar once nothing needs the room.
    assert_eq!(fitted_height(500., 0., 0., 1000., false), BAR_HEIGHT);
    assert_eq!(fitted_height(260., 0., 480., 1000., false), 480.);
}

#[test]
fn a_conversation_shown_in_the_bar_gets_room_for_a_turn() {
    // From the bar's height: at least the conversation's minimum.
    assert_eq!(fitted_height(BAR_HEIGHT, 0., 0., 1000., true), MIN_HEIGHT);
    assert_eq!(fitted_height(BAR_HEIGHT, 100., 0., 1000., true), MIN_HEIGHT);
    // More than that when the transcript needs it.
    assert_eq!(fitted_height(BAR_HEIGHT, 300., 0., 1000., true), 364.);
    // The bar grows with what it shows (more lines, an error), exactly: it
    // sits on the bottom edge, so spare height would be a gap above it.
    assert_eq!(fitted_height(BAR_HEIGHT, 0., 91., 1000., false), 91.);
}

// quick-panel / Opens with motion
#[test]
fn the_opening_fades_in_eased_and_ends_complete() {
    assert_eq!(open_opacity(0.), 0.);
    assert_eq!(open_opacity(1.), 1.);
    // Eased at both ends: slow off the start and into full opacity.
    assert!(open_opacity(0.05) < 0.05);
    let arrive = 0.64;
    assert!(open_opacity(arrive - 0.02) > 0.98);
    assert_eq!(open_opacity(arrive), 1., "full when the panel arrives");
    let mut last = 0.;
    for i in 0..=100 {
        let o = open_opacity(i as f32 / 100.);
        assert!(o >= last, "never dims on the way in");
        last = o;
    }
}

#[test]
fn the_opening_bumps_on_arrival_and_never_outgrows_the_window() {
    assert!((open_scale(0.) - 0.94).abs() < 1e-6);
    assert_eq!(open_scale(1.), 1.);
    let arrive = 0.64;
    assert!(
        (open_scale(arrive) - 1.).abs() < 1e-5,
        "at full size on arrival"
    );
    // It gathers speed into its size: more ground in the second half.
    let (half, full) = (open_scale(arrive / 2.), open_scale(arrive));
    assert!(full - half > half - open_scale(0.));
    // Then it springs back a little and settles.
    let lowest = (0..=100)
        .map(|i| open_scale(arrive + (1. - arrive) * i as f32 / 100.))
        .fold(1., f32::min);
    assert!(lowest < 0.99 && lowest > 0.98, "a small bump, got {lowest}");
    assert!(open_scale(0.99) > 0.9995, "slow into rest");
    for i in 0..=200 {
        assert!(
            open_scale(i as f32 / 200.) <= 1.,
            "the window would clip it"
        );
    }
}

#[gpui_kit::test]
fn panel_grows_with_its_transcript_and_stops_at_the_cap(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    assert_eq!(panel_height(cx, &panel), BAR_HEIGHT);

    // A short conversation gets the room of one, and little more.
    show_view(cx, &rig, &panel, long_view("short", 1, "idle"));
    settle(cx, window);
    let short = panel_height(cx, &panel);
    assert!(short >= MIN_HEIGHT, "room for a turn, got {short}");
    // (The window now reports its resizes back, as a real one does, so this
    // is the height one turn really takes.)
    assert!(short <= 320., "a short chat needs little room, got {short}");

    // A conversation that overflows grows it, one step at a time, to the cap.
    show_view(cx, &rig, &panel, long_view("long", 80, "idle"));
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), 700., "70 % of a 1000 px display");
    // The window itself is that tall, not just the request.
    assert_eq!(window_height(cx, window), 700.);
}

#[gpui_kit::test]
fn panel_never_shrinks_mid_conversation(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    show_view(cx, &rig, &panel, long_view("long", 80, "idle"));
    settle(cx, window);
    let grown = panel_height(cx, &panel);
    assert!(grown > MIN_HEIGHT);

    // A shorter conversation in the same panel does not shrink it.
    show_view(cx, &rig, &panel, long_view("short", 1, "idle"));
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), grown);
}

#[gpui_kit::test]
fn new_chat_returns_to_the_compact_height(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    show_view(cx, &rig, &panel, long_view("long", 80, "idle"));
    settle(cx, window);
    assert!(panel_height(cx, &panel) > MIN_HEIGHT);

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.new_chat(window, cx))
    })
    .unwrap();
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), BAR_HEIGHT);
    assert_eq!(window_height(cx, window), BAR_HEIGHT);
}

#[gpui_kit::test]
fn open_picker_raises_the_height_floor(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let sessions = (0..6)
        .map(|i| {
            summary(
                &format!("s{i}"),
                &format!("Session {i}"),
                "idle",
                &i.to_string(),
            )
        })
        .collect();
    rig.summaries(cx, sessions);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), BAR_HEIGHT);

    cx.update(|cx| panel.update(cx, |p, cx| p.set_picker_for_test(true, cx)));
    settle(cx, window);
    let open = panel_height(cx, &panel);
    assert!(
        open > MIN_HEIGHT,
        "the picker's six rows need room, got {open}"
    );

    // Closing it on an empty chat gives the room back.
    cx.update(|cx| panel.update(cx, |p, cx| p.set_picker_for_test(false, cx)));
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), BAR_HEIGHT);
}

#[gpui_kit::test]
fn glow_renders_only_while_the_session_runs(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    settle(cx, window);
    assert_eq!(panel.read_with(cx, |p, _| p.ring_frame_for_test()), None);

    let session = show_view(cx, &rig, &panel, long_view("run", 1, "running"));
    after_the_delay(cx, window);
    let frame = panel
        .read_with(cx, |p, _| p.ring_frame_for_test())
        .expect("drawn while the session runs");
    assert!(frame.highlight > 0., "the highlight travels");

    cx.update(|cx| {
        session.update(cx, |s, cx| {
            s.view.phase = "idle".into();
            cx.notify();
        })
    });
    // The short run fades out, then nothing is drawn.
    after_the_delay(cx, window);
    after_the_delay(cx, window);
    assert_eq!(panel.read_with(cx, |p, _| p.ring_frame_for_test()), None);
}

/// Let the opening animation end: while it runs the panel is laid out a
/// little smaller than its window.
fn finish_opening(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.executor().advance_clock(Duration::from_millis(400));
    settle(cx, window);
}

/// Let the clock pass the light's delay (and a fade), and render.
fn after_the_delay(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.executor().advance_clock(Duration::from_secs_f64(
        MOTION.delay.max(MOTION.fade) + 0.05,
    ));
    settle(cx, window);
}

#[gpui_kit::test]
fn idle_panel_requests_no_animation_frames(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("idle", 1, "idle"));
    // Let the opening animation end.
    cx.executor().advance_clock(Duration::from_millis(400));
    settle(cx, window);
    let frames = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx)
        })
        .unwrap();
    assert_eq!(frames, 0, "nothing animates in an idle panel");
}

#[gpui_kit::test]
fn reduce_motion_keeps_the_glow_still(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("run", 1, "running"));
    after_the_delay(cx, window);
    let first = panel
        .read_with(cx, |p, _| p.ring_frame_for_test())
        .expect("drawn");
    cx.executor().advance_clock(Duration::from_millis(500));
    settle(cx, window);
    let later = panel
        .read_with(cx, |p, _| p.ring_frame_for_test())
        .expect("drawn");
    assert_eq!(first, later, "the edge does not move");
    assert_eq!(first.highlight, 0., "no travelling highlight");
    assert!(first.line >= 0.3, "but it is lit");
    // And it asks for no frames.
    let frames = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx)
        })
        .unwrap();
    assert_eq!(frames, 0);
}

/// A renderer of the light outside GPUI, for tests: it records what it is
/// shown, and fails every call when asked to.
struct FakeLight {
    fail: bool,
    shown: Rc<RefCell<Vec<Show>>>,
}

impl EdgeLight for FakeLight {
    fn show(&mut self, state: &EdgeState, _: &EdgeColours) -> Result<(), EdgeError> {
        if self.fail {
            return Err(EdgeError("no layer".into()));
        }
        let mut shown = self.shown.borrow_mut();
        if shown.last() != Some(&state.part.show) {
            shown.push(state.part.show);
        }
        Ok(())
    }

    fn resize(&mut self, _: f32, _: f32) -> Result<(), EdgeError> {
        if self.fail {
            return Err(EdgeError("no layer".into()));
        }
        Ok(())
    }
}

fn with_fake_light(
    cx: &mut TestAppContext,
    panel: &Entity<QuickPanel>,
    fail: bool,
) -> Rc<RefCell<Vec<Show>>> {
    let shown = Rc::new(RefCell::new(Vec::new()));
    let light = FakeLight {
        fail,
        shown: shown.clone(),
    };
    panel.update(cx, |p, _| p.set_edge_light_for_test(Box::new(light)));
    shown
}

// quick-panel / The macOS light cannot run
#[gpui_kit::test]
fn a_failing_edge_light_hands_over_to_the_ring(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    with_fake_light(cx, &panel, true);
    show_view(cx, &rig, &panel, long_view("run", 1, "running"));
    after_the_delay(cx, window);
    assert!(!panel.read_with(cx, |p, _| p.has_edge_light_for_test()));
    assert!(
        panel
            .read_with(cx, |p, _| p.ring_frame_for_test())
            .is_some(),
        "the ring is drawn while the session runs"
    );
}

#[gpui_kit::test]
fn a_working_edge_light_replaces_the_ring_and_gets_the_states_in_order(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    let shown = with_fake_light(cx, &panel, false);
    let session = show_view(cx, &rig, &panel, long_view("run", 1, "running"));
    after_the_delay(cx, window);
    assert_eq!(panel.read_with(cx, |p, _| p.ring_frame_for_test()), None);
    // Waiting for the user, then a long enough run that ends cleanly.
    cx.update(|cx| {
        session.update(cx, |s, cx| {
            s.view.phase = "waiting".into();
            cx.notify();
        })
    });
    after_the_delay(cx, window);
    cx.update(|cx| {
        session.update(cx, |s, cx| {
            s.view.phase = "model".into();
            cx.notify();
        })
    });
    cx.executor().advance_clock(Duration::from_millis(1200));
    settle(cx, window);
    cx.update(|cx| {
        session.update(cx, |s, cx| {
            s.view.phase = "idle".into();
            cx.notify();
        })
    });
    settle(cx, window);
    cx.executor().advance_clock(Duration::from_secs(2));
    settle(cx, window);
    // Off first: the renderer is given its starting state with its colours.
    assert_eq!(
        *shown.borrow(),
        [
            Show::Off,
            Show::Travel,
            Show::Breathe,
            Show::Travel,
            Show::Finish,
            Show::Off
        ]
    );
    assert!(panel.read_with(cx, |p, _| p.has_edge_light_for_test()));
    assert_eq!(panel.read_with(cx, |p, _| p.ring_frame_for_test()), None);
}

#[gpui_kit::test]
fn reduce_motion_skips_the_opening_animation(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (window, panel) = rig.open_panel(cx);
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert_eq!(panel.read_with(cx, |p, _| p.opening_for_test()), 1.0);
}

#[gpui_kit::test]
fn the_opening_animation_runs_when_motion_is_allowed(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(panel.read_with(cx, |p, _| p.opening_for_test()) < 1.0);
    cx.executor().advance_clock(Duration::from_millis(300));
    settle(cx, window);
    assert_eq!(panel.read_with(cx, |p, _| p.opening_for_test()), 1.0);
}

#[gpui_kit::test]
fn keystrokes_during_the_opening_animation_reach_the_composer(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(
        panel.read_with(cx, |p, _| p.opening_for_test()) < 1.0,
        "still animating"
    );
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.simulate_input("hello");
    assert_eq!(draft(cx, &panel), "hello");
}

#[gpui_kit::test]
fn glass_chrome_is_only_reported_with_a_material(cx: &mut TestAppContext) {
    // The test platform has no window material, so the panel is opaque.
    let rig = rig(cx);
    let (_, panel) = rig.open_panel(cx);
    assert!(!panel.read_with(cx, |p, _| p.chrome_for_test().glass));
}

// ---- the bar of a new quick chat (design D15) -------------------------------

fn bounds_of(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    selector: &'static str,
) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
    VisualTestContext::from_window(window, cx).debug_bounds(selector)
}

// quick-panel / Looks like a HUD: a new quick chat is one bar, with no header.
#[gpui_kit::test]
fn a_new_quick_chat_is_one_bar_and_a_conversation_has_the_header(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), BAR_HEIGHT);
    assert!(bounds_of(cx, window, "quick-header").is_none());
    let bar = bounds_of(cx, window, "quick-bar").expect("the bar shows");
    let input = bounds_of(cx, window, "quick-composer").expect("with the input in it");
    // One line of large text at rest: a 26 px line with no padding of the
    // kit's around it (design D19), where the conversation's composer has
    // 20 px lines.
    let line = f32::from(input.size.height);
    assert!((25. ..27.).contains(&line), "one 26 px line, got {line}");
    assert!(f32::from(bar.size.height) <= BAR_HEIGHT);

    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-header").is_some());
    assert!(bounds_of(cx, window, "quick-bar").is_none());
    assert!(panel_height(cx, &panel) >= MIN_HEIGHT);
}

// quick-panel / Grows with the conversation: tall enough for its errors.
#[gpui_kit::test]
fn an_error_above_the_bar_raises_the_panel(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| {
        rig.store.update(cx, |s, _| {
            s.runtime_mut_for_test("test-agent").state = ConnState::Idle;
        })
    });
    let (window, panel) = rig.open_panel(cx);
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), BAR_HEIGHT);
    type_draft(cx, window, &panel, "are you there?");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.submit(window, cx))
    })
    .unwrap();
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-error").is_some());
    let raised = panel_height(cx, &panel);
    assert!(raised > BAR_HEIGHT, "room for the error, got {raised}");
    assert!(
        raised < MIN_HEIGHT,
        "and no more than it needs, got {raised}"
    );
}

// The draft of a new quick chat carries into the conversation's composer.
// Both rest at one line, so the placeholder, the buttons and the chip share a
// centre line; the composer's text is smaller than the bar's.
#[gpui_kit::test]
fn the_bar_and_the_composer_rest_at_one_line(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    settle(cx, window);
    let bar = bounds_of(cx, window, "quick-composer")
        .expect("the bar's input")
        .size
        .height;
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    let composer = bounds_of(cx, window, "composer-input")
        .expect("the composer's input")
        .size
        .height;
    // One 20 px line is shorter than one 26 px line, not two lines tall. The
    // composer keeps the kit's 8 px above and below its lines, which the
    // bar's input drops.
    let padding = gpui_kit::px(16.);
    assert!(composer <= bar + padding, "{composer:?} vs {bar:?}");
}

// ---- model switching (design D12) -------------------------------------------

fn model(id: &str, label: &str) -> crate::store::Model {
    crate::store::Model {
        id: id.into(),
        label: label.into(),
        provider: "prov".into(),
        model: id.into(),
        node_id: None,
        node: None,
        context: None,
        modalities: Vec::new(),
        raw: json!({ "id": id }),
    }
}

fn with_models(rig: &Rig, cx: &mut TestAppContext) {
    cx.update(|cx| {
        rig.store.update(cx, |s, _| {
            s.runtime_mut_for_test("test-agent").models =
                vec![model("m-a", "Model A"), model("m-b", "Model B")];
        })
    });
}

fn pick_model(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
    key: &str,
) {
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
    let picker = panel.read_with(cx, |p, _| p.models_for_test().clone());
    assert!(picker.read_with(cx, |p, _| p.is_open()));
    cx.update(|cx| picker.update(cx, |p, cx| p.pick(key.into(), cx)));
    cx.run_until_parked();
}

#[gpui_kit::test]
fn panel_model_choice_starts_the_quick_chat_on_it_without_changing_the_launch_default(
    cx: &mut TestAppContext,
) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let default_before = rig
        .store
        .read_with(cx, |s, _| s.prefs.launch_models.clone());
    let (window, panel) = rig.open_panel(cx);
    pick_model(cx, window, &panel, "m-b");

    // The panel remembers it, the picker closed, the start page's default is untouched.
    assert_eq!(
        panel
            .read_with(cx, |p, _| p.model_for_test().map(str::to_owned))
            .as_deref(),
        Some("m-b")
    );
    let picker = panel.read_with(cx, |p, _| p.models_for_test().clone());
    assert!(!picker.read_with(cx, |p, _| p.is_open()));
    assert_eq!(
        rig.store
            .read_with(cx, |s, _| s.prefs.launch_models.clone()),
        default_before
    );

    type_draft(cx, window, &panel, "hello");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.submit(window, cx))
    })
    .unwrap();
    let sent = rig.pump(cx);
    let set = sent
        .iter()
        .find(|r| r["method"] == "session/set_config_option" && r["params"]["configId"] == "model")
        .expect("the session starts on a model");
    assert_eq!(set["params"]["value"], "m-b");
    assert_eq!(
        rig.store
            .read_with(cx, |s, _| s.prefs.launch_models.clone()),
        default_before,
        "still the start page's default"
    );
}

#[gpui_kit::test]
fn panel_model_choice_is_kept_while_the_panel_is_closed(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let (window, panel) = rig.open_panel(cx);
    pick_model(cx, window, &panel, "m-b");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.dismiss(window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    rig.host.read_with(cx, |h, _| {
        assert_eq!(h.state().picks.model.as_deref(), Some("m-b"))
    });
    let (_, panel) = rig.open_panel(cx);
    assert_eq!(
        panel
            .read_with(cx, |p, _| p.model_for_test().map(str::to_owned))
            .as_deref(),
        Some("m-b")
    );
}

#[gpui_kit::test]
fn panel_model_choice_inside_a_session_sets_the_session_option(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let a = summary("a", "Session A", "idle", "2");
    rig.summaries(cx, vec![a.clone()]);
    let (window, panel) = rig.open_panel(cx);
    pick(cx, window, &panel, &a);
    rig.pump(cx);

    pick_model(cx, window, &panel, "m-b");
    let sent = rig.sent(cx);
    let set = sent
        .iter()
        .find(|r| r["method"] == "session/set_config_option")
        .expect("the session's model option is set");
    assert_eq!(set["params"]["sessionId"], "a");
    assert_eq!(set["params"]["configId"], "model");
    assert_eq!(set["params"]["value"], "m-b");
    // That is the session's model: nothing is kept for the next quick chat.
    assert_eq!(
        panel.read_with(cx, |p, _| p.model_for_test().map(str::to_owned)),
        None
    );
}

#[gpui_kit::test]
fn escape_closes_the_model_picker_before_the_panel(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let (window, panel) = rig.open_panel(cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!closed(cx, &rig), "the first Esc closes the picker only");
    let picker = panel.read_with(cx, |p, _| p.models_for_test().clone());
    assert!(!picker.read_with(cx, |p, _| p.is_open()));
}

fn with_many_models(rig: &Rig, cx: &mut TestAppContext, n: usize) {
    cx.update(|cx| {
        rig.store.update(cx, |s, _| {
            s.runtime_mut_for_test("test-agent").models = (0..n)
                .map(|i| model(&format!("m-{i}"), &format!("Model {i}")))
                .collect();
        })
    });
}

/// Every model row is shown: the rows' box does not scroll.
fn every_model_shown(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> bool {
    panel.read_with(cx, |p, cx| {
        p.models_for_test()
            .read(cx)
            .rows_scroll_for_test()
            .max_offset()
            .y
            <= gpui_kit::px(0.5)
    })
}

// quick-panel / Picker in a small panel: the floor is what the picker lays
// out to (design D13), not an estimate. It makes room for five models; a
// longer list scrolls.
#[gpui_kit::test]
fn open_model_picker_raises_the_height_floor(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_many_models(&rig, cx, 3);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
    settle(cx, window);
    finish_opening(cx, window);
    let height = panel_height(cx, &panel);
    let sheet = bounds_of(cx, window, "quick-models").expect("the picker");
    assert!(bottom(sheet) <= height + 0.5, "{sheet:?} in {height}");
    assert!(every_model_shown(cx, &panel), "the rows do not scroll");
    // In a new quick chat it lies above the bar.
    let bar = bounds_of(cx, window, "quick-bar").expect("the bar");
    let sheet = bounds_of(cx, window, "quick-models").expect("the picker");
    assert!(bottom(sheet) <= top(bar) + 0.5, "{sheet:?} above {bar:?}");
}

// quick-panel / Picker in a small panel: a long model list does not make the
// panel as tall as the display; the rows past the fifth scroll.
#[gpui_kit::test]
fn a_long_model_list_scrolls_in_a_short_sheet(cx: &mut TestAppContext) {
    let few = {
        let rig = rig(cx);
        with_many_models(&rig, cx, 5);
        let (window, panel) = rig.open_panel(cx);
        panel.update(cx, |p, _| p.set_display_height_for_test(1400.));
        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |p, cx| p.open_model_picker(window, cx))
        })
        .unwrap();
        settle(cx, window);
        finish_opening(cx, window);
        assert!(every_model_shown(cx, &panel), "five models fit");
        panel_height(cx, &panel)
    };
    let rig = rig(cx);
    with_many_models(&rig, cx, 40);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1400.));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
    settle(cx, window);
    finish_opening(cx, window);
    assert!(!every_model_shown(cx, &panel), "the rest scroll");
    let height = panel_height(cx, &panel);
    assert!(
        height < few + 80.,
        "{height} is about the height of five models ({few}), not the display's"
    );
    let sheet = bounds_of(cx, window, "quick-models").expect("the picker");
    assert!(bottom(sheet) <= height + 0.5, "{sheet:?} in {height}");
}

// quick-panel / The model picker is a list: a search field and the models,
// with no title, reasoning control, close button or Done (design D20), for a
// launch target (a new quick chat) and a session target (a conversation).
#[gpui_kit::test]
fn the_model_sheet_is_search_and_list(cx: &mut TestAppContext) {
    for in_conversation in [false, true] {
        let rig = rig(cx);
        with_many_models(&rig, cx, 4);
        let (window, panel) = rig.open_panel(cx);
        panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
        if in_conversation {
            show_view(cx, &rig, &panel, long_view("long", 20, "idle"));
            settle(cx, window);
        }
        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |p, cx| p.open_model_picker(window, cx))
        })
        .unwrap();
        settle(cx, window);
        finish_opening(cx, window);
        assert!(
            bounds_of(cx, window, "quick-models").is_some(),
            "the sheet is open (conversation: {in_conversation})"
        );
        for id in ["model-done", "model-close", "model-reasoning"] {
            assert!(
                bounds_of(cx, window, id).is_none(),
                "{id} is absent (conversation: {in_conversation})"
            );
        }
        assert!(every_model_shown(cx, &panel), "the list shows the models");
    }
}

// quick-panel / List in place of the conversation: the model picker
#[gpui_kit::test]
fn the_model_picker_replaces_the_conversation(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_many_models(&rig, cx, 4);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    show_view(cx, &rig, &panel, long_view("long", 20, "idle"));
    settle(cx, window);
    assert!(bounds_of(cx, window, "composer-highlight").is_some());
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
    settle(cx, window);
    finish_opening(cx, window);
    assert!(
        bounds_of(cx, window, "composer-highlight").is_none(),
        "the conversation gave its place to the picker"
    );
    let header = bounds_of(cx, window, "quick-header").expect("the header stays");
    let sheet = bounds_of(cx, window, "quick-models").expect("the picker");
    assert!(
        top(sheet) >= bottom(header) - 0.5,
        "{sheet:?} under {header:?}"
    );
    assert!(every_model_shown(cx, &panel));
    press(cx, window, "escape");
    assert!(
        bounds_of(cx, window, "composer-highlight").is_some(),
        "back"
    );
}

// ---- keyboard (design D16) ----------------------------------------------------

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .unwrap();
    cx.run_until_parked();
}

/// The shortcut for `key` with the platform's main modifier.
fn secondary(cx: &mut TestAppContext, key: &str) -> String {
    let mac = cx.update(|cx| cx.platform_info().is_mac());
    format!("{}-{key}", if mac { "cmd" } else { "ctrl" })
}

fn bind_keys(cx: &mut TestAppContext) {
    cx.update(crate::ui::bind_keys);
}

fn picked_model(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> Option<String> {
    panel.read_with(cx, |p, _| p.model_for_test().map(str::to_owned))
}

fn open_models(cx: &mut TestAppContext, window: AnyWindowHandle, panel: &Entity<QuickPanel>) {
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_arrows_move_through_the_models_and_enter_picks_the_one_marked(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let (window, panel) = rig.open_panel(cx);
    open_models(cx, window, &panel);
    press(cx, window, "down");
    press(cx, window, "enter");
    assert_eq!(picked_model(cx, &panel).as_deref(), Some("m-b"));
}

#[gpui_kit::test]
fn enter_alone_picks_the_first_model_and_up_wraps_to_the_last(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let (window, panel) = rig.open_panel(cx);
    open_models(cx, window, &panel);
    press(cx, window, "enter");
    assert_eq!(picked_model(cx, &panel).as_deref(), Some("m-a"));

    open_models(cx, window, &panel);
    press(cx, window, "up");
    press(cx, window, "enter");
    assert_eq!(picked_model(cx, &panel).as_deref(), Some("m-b"));
}

#[gpui_kit::test]
fn the_session_list_takes_the_arrows_and_enter(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("a", "Session A", "running", "2"),
            summary("b", "Session B", "idle", "1"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.toggle_picker_for_test(window, cx))
    })
    .unwrap();
    press(cx, window, "down");
    press(cx, window, "enter");
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("b"));
}

#[gpui_kit::test]
fn the_main_modifier_and_a_digit_shows_that_row_of_the_session_list(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("a", "Session A", "running", "2"),
            summary("b", "Session B", "idle", "1"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    // Closed, the shortcut is nothing here.
    let second = secondary(cx, "2");
    press(cx, window, &second);
    assert_eq!(pane_key(cx, &panel), None);

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.toggle_picker_for_test(window, cx))
    })
    .unwrap();
    press(cx, window, &second);
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("b"));
}

fn composer_focused(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        panel
            .read(cx)
            .pane_for_test()
            .read(cx)
            .composer_focused_for_test(window, cx)
    })
    .unwrap()
}

fn list_search_focused(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        let query = panel.read(cx).picker_query_for_test().clone();
        gpui_kit::Focusable::focus_handle(&query, cx).is_focused(window)
    })
    .unwrap()
}

// quick-panel / Session list by keyboard: opens with the search focused,
// closes again with the composer focused, from the composer and from the
// list's own search field.
#[gpui_kit::test]
fn cmd_k_opens_and_closes_the_session_list(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    rig.summaries(cx, vec![summary("a", "Session A", "idle", "1")]);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| {
            p.pane_for_test()
                .update(cx, |pane, cx| pane.focus_composer(window, cx))
        })
    })
    .unwrap();
    assert!(composer_focused(cx, window, &panel));

    let key = secondary(cx, "k");
    press(cx, window, &key);
    assert!(panel.read_with(cx, |p, _| p.picker_open_for_test()));
    assert!(list_search_focused(cx, window, &panel));

    // The same key closes it from the search field.
    press(cx, window, &key);
    assert!(!panel.read_with(cx, |p, _| p.picker_open_for_test()));
    assert!(composer_focused(cx, window, &panel));
}

#[gpui_kit::test]
fn cmd_k_opens_the_session_list_from_a_new_quick_chats_bar(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    let key = secondary(cx, "k");
    press(cx, window, &key);
    assert!(panel.read_with(cx, |p, _| p.picker_open_for_test()));
    press(cx, window, &key);
    assert!(!panel.read_with(cx, |p, _| p.picker_open_for_test()));
}

/// Which sheets are open: sessions, models, folder.
fn sheets(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> (bool, bool, bool) {
    panel.read_with(cx, |p, cx| {
        (
            p.picker_open_for_test(),
            p.models_for_test().read(cx).is_open(),
            p.folder_open_for_test(),
        )
    })
}

fn keys_reach_panel(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        panel.read(cx).keys_reach_panel_for_test(window, cx)
    })
    .unwrap()
}

/// Presses `keys` one after another; after each, at most one sheet is open
/// and the keyboard is inside the panel, so the next key is not lost.
fn press_all(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
    keys: &[&str],
) {
    for key in keys {
        press(cx, window, key);
        settle(cx, window);
        let (sessions, models, folder) = sheets(cx, panel);
        assert!(
            [sessions, models, folder].iter().filter(|o| **o).count() <= 1,
            "after {key}: sessions {sessions}, models {models}, folder {folder}"
        );
        assert!(keys_reach_panel(cx, window, panel), "after {key}");
    }
}

// quick-panel / Session list by keyboard: Cmd+M and Cmd+K in any order leave
// one sheet open and the keyboard working; Esc then closes the sheet and the
// panel, without the mouse.
fn alternating_cmd_m_and_cmd_k(cx: &mut TestAppContext, session: bool) {
    bind_keys(cx);
    let rig = rig(cx);
    with_models(&rig, cx);
    rig.summaries(cx, vec![summary("a", "Session A", "idle", "1")]);
    let (window, panel) = rig.open_panel(cx);
    if session {
        show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    }
    settle(cx, window);
    let (m, k) = (secondary(cx, "m"), secondary(cx, "k"));

    // Each key opens its sheet, and the sheet it finds open closes.
    press_all(cx, window, &panel, &[&m]);
    assert_eq!(sheets(cx, &panel), (false, true, false));
    press_all(cx, window, &panel, &[&k]);
    assert_eq!(sheets(cx, &panel), (true, false, false), "K replaces M");
    press_all(cx, window, &panel, &[&m]);
    assert_eq!(sheets(cx, &panel), (false, true, false), "M replaces K");
    press_all(cx, window, &panel, &[&m]);
    assert_eq!(sheets(cx, &panel), (false, false, false), "M closes M");
    assert!(composer_focused(cx, window, &panel));
    press_all(cx, window, &panel, &[&k, &k]);
    assert_eq!(sheets(cx, &panel), (false, false, false), "K closes K");
    assert!(composer_focused(cx, window, &panel));

    // A long alternating run, as it was found by hand.
    press_all(cx, window, &panel, &[&m, &k, &m, &k, &m, &k, &k, &m, &k]);
    assert_eq!(sheets(cx, &panel), (true, false, false));

    // Esc closes the sheet, the next Esc the panel.
    press(cx, window, "escape");
    assert_eq!(sheets(cx, &panel), (false, false, false));
    assert!(composer_focused(cx, window, &panel));
    assert!(!closed(cx, &rig));
    press(cx, window, "escape");
    assert!(closed(cx, &rig), "the second Esc closes the panel");
}

#[gpui_kit::test]
fn alternating_cmd_m_and_cmd_k_keeps_the_keyboard_in_a_conversation(cx: &mut TestAppContext) {
    alternating_cmd_m_and_cmd_k(cx, true);
}

#[gpui_kit::test]
fn alternating_cmd_m_and_cmd_k_keeps_the_keyboard_in_a_new_quick_chat(cx: &mut TestAppContext) {
    alternating_cmd_m_and_cmd_k(cx, false);
}

/// A read-only session (archived, handed off): no composer.
fn read_only(mut view: SessionView) -> SessionView {
    view.access = Some(crate::session::SessionAccess {
        reason: None,
        owner_session_id: None,
    });
    view
}

/// Run `f` on the panel and then what waits for the next frame, with no
/// frame drawn in between: the platform's order. The harness's own helpers
/// draw after an input, which hides a check that looks at the frame from
/// before it.
fn then_next_frame(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
    f: impl FnOnce(&mut QuickPanel, &mut gpui_kit::Window, &mut gpui_kit::Context<QuickPanel>),
) {
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| f(p, window, cx));
        window.simulate_next_frame(cx);
    })
    .unwrap();
}

// quick-panel / Session list by keyboard: Cmd/Ctrl+K puts the keyboard on
// the list's search, so typing filters and the arrows move the cursor. Found
// by hand: a focus check scheduled before the list was drawn found its
// search missing and took the keyboard.
#[gpui_kit::test]
fn cmd_k_puts_the_keyboard_on_the_search(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("a", "Alpha", "idle", "3"),
            summary("b", "Beta", "idle", "2"),
            summary("c", "Gamma", "idle", "1"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    // A frame drawn just before the key (another session shown) leaves a
    // focus check waiting; it looks at that frame, without the list.
    show_view(cx, &rig, &panel, long_view("other", 1, "idle"));
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    // The key, then the next frame's callbacks, before the list is drawn.
    then_next_frame(cx, window, &panel, |p, window, cx| {
        p.toggle_picker_for_test(window, cx)
    });
    assert!(
        list_search_focused(cx, window, &panel),
        "the search keeps the keyboard until the list is drawn"
    );
    settle(cx, window);
    assert!(list_search_focused(cx, window, &panel), "and after");
    // Typing filters.
    VisualTestContext::from_window(window, cx).simulate_input("gam");
    settle(cx, window);
    assert_eq!(row_ids(cx, &panel), ["c"], "typing filters the list");
    // Clear, then Down moves the cursor.
    for _ in 0..3 {
        press(cx, window, "backspace");
    }
    settle(cx, window);
    press(cx, window, "down");
    assert_eq!(panel.read_with(cx, |p, _| p.picker_cursor_for_test()), 1);
}

// A read-only session shows no composer: opened from the list, the panel
// keeps the keyboard, so Esc still closes it. Found by hand with an archived
// worktree session.
#[gpui_kit::test]
fn esc_closes_the_panel_over_a_read_only_session_picked_from_the_list(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    // The archived session is the newest, so the list's cursor starts on it.
    rig.summaries(
        cx,
        vec![
            summary("a", "Alpha", "idle", "3"),
            summary("archived", "Archived", "idle", "9"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    // Held: the store shares an open session only while someone holds it.
    let _archived = cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.adopt_for_test(read_only(long_view("archived", 1, "idle")), cx)
        })
    });
    show_view(cx, &rig, &panel, long_view("a", 1, "idle"));
    settle(cx, window);
    let k = secondary(cx, "k");
    press(cx, window, &k);
    settle(cx, window);
    assert!(list_search_focused(cx, window, &panel));
    assert_eq!(row_ids(cx, &panel), ["archived", "a"]);
    // Enter on the row shows the read-only session; the list closes.
    press(cx, window, "enter");
    settle(cx, window);
    assert_eq!(
        pane_key(cx, &panel).as_deref(),
        Some("archived"),
        "the list's row was picked"
    );
    assert!(!panel.read_with(cx, |p, _| p.picker_open_for_test()));
    assert!(
        keys_reach_panel(cx, window, &panel),
        "the keyboard is in the panel"
    );
    press(cx, window, "escape");
    assert!(closed(cx, &rig), "Esc closes the panel");
}

// The panel opened on a read-only session: its composer is not shown, so the
// keyboard goes to the panel and Esc closes it.
#[gpui_kit::test]
fn esc_closes_the_panel_opened_on_a_read_only_session(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    // The window starts on the session, as one closed with it shown does.
    let archived = cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.adopt_for_test(read_only(long_view("archived", 1, "idle")), cx)
        })
    });
    cx.update(|cx| {
        rig.host
            .update(cx, |h, _| h.resume_session_for_test(archived.clone()))
    });
    let (window, panel) = rig.open_panel(cx);
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("archived"));
    settle(cx, window);
    assert!(
        keys_reach_panel(cx, window, &panel),
        "the keyboard is in the panel"
    );
    press(cx, window, "escape");
    assert!(closed(cx, &rig), "Esc closes the panel");
}

// A session that turns read-only once loaded (an archived worktree): its
// composer, which had the keyboard, goes away; the panel takes the keyboard
// and Esc still closes it.
#[gpui_kit::test]
fn esc_closes_the_panel_when_the_session_turns_read_only(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    let session = show_view(cx, &rig, &panel, long_view("loading", 1, "idle"));
    settle(cx, window);
    assert!(
        composer_focused(cx, window, &panel),
        "the composer to begin with"
    );
    // The load answers: read-only.
    session.update(cx, |s, cx| {
        s.view.access = Some(crate::session::SessionAccess {
            reason: None,
            owner_session_id: None,
        });
        cx.notify();
    });
    settle(cx, window);
    assert!(
        keys_reach_panel(cx, window, &panel),
        "the keyboard is in the panel"
    );
    press(cx, window, "escape");
    assert!(closed(cx, &rig), "Esc closes the panel");
}

// quick-panel / Session list by keyboard: a click in the list (a row, the
// glass) leaves the keyboard on its search, so the arrows keep working.
// Found by hand: the panel, focusable as the keyboard's last resort, took
// the focus on any click that nothing deeper took.
#[gpui_kit::test]
fn a_click_in_the_list_leaves_the_arrows_working(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("a", "Alpha", "idle", "3"),
            summary("b", "Beta", "idle", "2"),
            summary("c", "Gamma", "idle", "1"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    let k = secondary(cx, "k");
    press(cx, window, &k);
    settle(cx, window);
    assert!(list_search_focused(cx, window, &panel));
    // Press (no release, so nothing is picked) on the second row.
    let at = centre(bounds_dyn(cx, window, "quick-pick-row:1".into()).expect("row"));
    let mut vcx = VisualTestContext::from_window(window, cx);
    vcx.simulate_mouse_down(at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::none());
    cx.run_until_parked();
    settle(cx, window);
    assert!(
        list_search_focused(cx, window, &panel),
        "the click left the keyboard on the search"
    );
    let before = panel.read_with(cx, |p, _| p.picker_cursor_for_test());
    press(cx, window, "down");
    assert_eq!(
        panel.read_with(cx, |p, _| p.picker_cursor_for_test()),
        before + 1,
        "Down still moves the cursor"
    );
}

// The keyboard lands in the sheet that replaced another one.
#[gpui_kit::test]
fn the_sheet_that_replaces_another_has_the_keyboard(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    with_models(&rig, cx);
    rig.summaries(cx, vec![summary("a", "Session A", "idle", "1")]);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    let (m, k) = (secondary(cx, "m"), secondary(cx, "k"));
    press_all(cx, window, &panel, &[&m, &k]);
    assert!(list_search_focused(cx, window, &panel), "K from the models");
    // Down and Enter reach the list: the session of the row is shown.
    press(cx, window, "enter");
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("a"));
    assert!(composer_focused(cx, window, &panel));
}

// quick-panel / New quick chat from a conversation.
#[gpui_kit::test]
fn cmd_n_starts_a_new_quick_chat_in_the_panel(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    assert!(pane_key(cx, &panel).is_some());
    let main_page = rig.app.read_with(cx, |app, _| app.page_for_test());

    let key = secondary(cx, "n");
    press(cx, window, &key);
    settle(cx, window);
    assert_eq!(pane_key(cx, &panel), None, "an empty quick chat");
    assert!(bounds_of(cx, window, "quick-bar").is_some());
    assert!(composer_focused(cx, window, &panel));
    assert_eq!(
        rig.app.read_with(cx, |app, _| app.page_for_test()),
        main_page,
        "the main window is unchanged"
    );
}

/// `long_view` for an agent that offers Build, Plan and Review.
fn view_in_mode(id: &str, mode: &str) -> SessionView {
    let mut view = long_view(id, 1, "idle");
    view.apply_modes(Some(&json!({
        "currentModeId": mode,
        "availableModes": [
            { "id": "build", "name": "Build" },
            { "id": "plan", "name": "Plan" },
            { "id": "review", "name": "Review" },
        ],
    })));
    view
}

// quick-panel / Tab changes the mode visibly.
#[gpui_kit::test]
fn tab_in_the_compact_composer_moves_the_chip(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, view_in_mode("chat", "build"));
    settle(cx, window);
    assert!(bounds_of(cx, window, "composer-mode:Build").is_some());
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| {
            p.pane_for_test()
                .update(cx, |pane, cx| pane.focus_composer(window, cx))
        })
    })
    .unwrap();

    press(cx, window, "tab");
    let sent = rig.pump(cx);
    let set = sent
        .iter()
        .find(|r| r["method"] == "session/set_mode")
        .expect("the mode is changed on the agent");
    assert_eq!(set["params"]["modeId"], "plan");
    settle(cx, window);
    assert!(bounds_of(cx, window, "composer-mode:Plan").is_some());
    assert!(bounds_of(cx, window, "composer-mode:Build").is_none());

    // And back.
    press(cx, window, "shift-tab");
    let sent = rig.pump(cx);
    let set = sent
        .iter()
        .find(|r| r["method"] == "session/set_mode")
        .expect("and back");
    assert_eq!(set["params"]["modeId"], "build");
    settle(cx, window);
    assert!(bounds_of(cx, window, "composer-mode:Build").is_some());
}

/// `view_in_mode` on a model, with the reasoning effort `effort` among
/// low, medium, high and max (none when `effort` is `None`).
fn view_running_with(id: &str, mode: &str, effort: Option<&str>) -> SessionView {
    let mut view = view_in_mode(id, mode);
    view.model = Some("m-a".into());
    view.options = vec![json!({
        "id": "model", "type": "select", "currentValue": "m-a",
        "options": [{ "value": "m-a", "name": "Model A" }],
    })];
    if let Some(effort) = effort {
        view.options.push(json!({
            "id": "thought_level", "type": "select", "currentValue": effort,
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "medium", "name": "Medium" },
                { "value": "high", "name": "High" },
                { "value": "max", "name": "Max" },
            ],
        }));
    }
    view
}

// quick-panel / What a conversation runs with: shown at a glance, as text.
#[gpui_kit::test]
fn a_conversation_shows_its_model_effort_and_mode_as_text(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(
        cx,
        &rig,
        &panel,
        view_running_with("chat", "build", Some("high")),
    );
    settle(cx, window);
    let header = bounds_of(cx, window, "quick-header").expect("the header");
    let model = bounds_of(cx, window, "quick-run-model").expect("the model in the header");
    let effort = bounds_of(cx, window, "quick-run-effort").expect("the effort in the header");
    for part in [model, effort] {
        assert!(top(part) >= top(header) && bottom(part) <= bottom(header) + 0.5);
    }
    assert!(model.right() <= effort.left(), "{model:?} then {effort:?}");
    // Before the context meter / the open-in-main button, after the drag area.
    let open = bounds_of(cx, window, "quick-open-main");
    if let Some(open) = open {
        assert!(effort.right() <= open.left());
    }
    assert!(bounds_of(cx, window, "composer-mode:Build").is_some());
    assert!(
        bounds_of(cx, window, "composer-outline").is_some(),
        "the composer is outlined in the mode's tone"
    );
    assert!(
        bounds_of(cx, window, "composer-highlight").is_none(),
        "instead of the white highlight"
    );
}

// quick-panel / Changing the effort moves nothing: whatever effort is shown,
// the model and the effort keep their bounds in the header.
#[gpui_kit::test]
fn changing_the_effort_moves_nothing_in_the_header(cx: &mut TestAppContext) {
    let mut seen = None;
    for effort in ["low", "medium", "high", "max"] {
        let rig = rig(cx);
        let (window, panel) = rig.open_panel(cx);
        show_view(
            cx,
            &rig,
            &panel,
            view_running_with("chat", "build", Some(effort)),
        );
        settle(cx, window);
        let model = bounds_of(cx, window, "quick-run-model").expect("the model");
        let shown = bounds_of(cx, window, "quick-run-effort").expect("the effort");
        match seen {
            None => seen = Some((model, shown)),
            Some(first) => assert_eq!((model, shown), first, "moved at {effort}"),
        }
    }
}

// quick-panel / Compact conversation: the pill is one row, its attach button,
// input, mode and send button on the pill's centre line.
#[gpui_kit::test]
fn the_composer_row_is_centred_in_its_pill(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(
        cx,
        &rig,
        &panel,
        view_running_with("chat", "build", Some("high")),
    );
    settle(cx, window);
    let pill = bounds_of(cx, window, "composer-outline").expect("the pill");
    let middle = |b: gpui_kit::Bounds<gpui_kit::Pixels>| f32::from(b.center().y);
    for id in [
        "composer-attach",
        "composer-input",
        "composer-mode",
        "composer-send",
    ] {
        let part = bounds_of(cx, window, id).unwrap_or_else(|| panic!("{id} is drawn"));
        assert!(
            (middle(part) - middle(pill)).abs() <= 0.5,
            "{id} {part:?} is centred in the pill {pill:?}"
        );
    }
}

// quick-panel / No reasoning option: the header shows the model only.
#[gpui_kit::test]
fn without_a_reasoning_option_the_header_shows_the_model_only(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, view_running_with("chat", "build", None));
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-run-model").is_some());
    assert!(bounds_of(cx, window, "quick-run-effort").is_none());
}

// A session whose agent offers no modes keeps the white highlight.
#[gpui_kit::test]
fn without_modes_the_composer_keeps_its_highlight(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    assert!(bounds_of(cx, window, "composer-highlight").is_some());
    assert!(bounds_of(cx, window, "composer-outline").is_none());
    assert!(bounds_of(cx, window, "composer-mode").is_none());
}

// quick-panel / What a conversation runs with: clicking the model opens the
// model picker.
#[gpui_kit::test]
fn clicking_the_model_in_the_header_opens_the_sheet(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(
        cx,
        &rig,
        &panel,
        view_running_with("chat", "build", Some("high")),
    );
    settle(cx, window);
    let at = centre(bounds_of(cx, window, "quick-run-model").expect("the model"));
    press_at(cx, window, at);
    settle(cx, window);
    assert!(panel.read_with(cx, |p, cx| p.models_for_test().read(cx).is_open()));
    assert_eq!(drags(cx, &panel), 0, "the text is not a drag area");
}

// quick-panel / What a conversation runs with: clicking the effort sends the
// next one, and nothing while a change is pending.
#[gpui_kit::test]
fn clicking_the_effort_in_the_header_sends_the_next_one(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(
        cx,
        &rig,
        &panel,
        view_running_with("chat", "build", Some("high")),
    );
    settle(cx, window);
    rig.pump(cx);
    let at = centre(bounds_of(cx, window, "quick-run-effort").expect("the effort"));
    press_at(cx, window, at);
    let sent = rig.pump(cx);
    let set = sent
        .iter()
        .find(|r| r["method"] == "session/set_config_option")
        .expect("the next effort is sent");
    assert_eq!(set["params"]["configId"], "thought_level");
    assert_eq!(set["params"]["value"], "max");
}

// quick-panel / What a conversation runs with: while a change is pending the
// effort is inert.
#[gpui_kit::test]
fn the_effort_is_inert_while_a_change_is_pending(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    let mut view = view_running_with("chat", "build", Some("high"));
    view.pending_options.insert("thought_level".into());
    show_view(cx, &rig, &panel, view);
    settle(cx, window);
    rig.pump(cx);
    let at = centre(bounds_of(cx, window, "quick-run-effort").expect("the effort"));
    press_at(cx, window, at);
    let sent = rig.pump(cx);
    assert!(
        sent.iter()
            .all(|r| r["method"] != "session/set_config_option"),
        "inert while pending: {sent:?}"
    );
}

// quick-panel / Changing the mode moves nothing: through every mode the input
// and the send button keep their bounds, and so does the composer's pill.
#[gpui_kit::test]
fn tab_through_every_mode_leaves_the_input_and_send_where_they_are(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(
        cx,
        &rig,
        &panel,
        view_running_with("chat", "build", Some("high")),
    );
    settle(cx, window);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| {
            p.pane_for_test()
                .update(cx, |pane, cx| pane.focus_composer(window, cx))
        })
    })
    .unwrap();
    let layout = |cx: &mut TestAppContext| {
        (
            bounds_of(cx, window, "composer-input").expect("the input"),
            bounds_of(cx, window, "composer-send").expect("the send button"),
            bounds_of(cx, window, "composer-mode").expect("the mode"),
            bounds_of(cx, window, "composer-outline").expect("the pill"),
        )
    };
    let before = layout(cx);
    for name in ["Plan", "Review", "Build"] {
        press(cx, window, "tab");
        rig.pump(cx);
        settle(cx, window);
        assert_eq!(layout(cx), before, "nothing moved at {name}");
    }
}

/// `long_view` that has used `used` of `size` tokens of its context.
fn view_with_usage(used: u64, size: u64) -> SessionView {
    let mut view = long_view("chat", 1, "idle");
    view.usage = Some((used, size));
    view
}

// quick-panel / Context half full: a ring, no percentage.
#[gpui_kit::test]
fn header_shows_the_context_ring(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, view_with_usage(50_000, 100_000));
    settle(cx, window);
    let ring = bounds_of(cx, window, "quick-context").expect("the ring shows");
    let header = bounds_of(cx, window, "quick-header").expect("in the header");
    assert!(ring.top() >= header.top() && ring.bottom() <= header.bottom());
    assert!(bounds_of(cx, window, "quick-context-percent").is_none());
}

// quick-panel / Context nearly full: the percentage too.
#[gpui_kit::test]
fn percentage_shows_when_nearly_full(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, view_with_usage(92_000, 100_000));
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-context").is_some());
    assert!(bounds_of(cx, window, "quick-context-percent").is_some());
}

// Just under 70% it is the ring alone.
#[gpui_kit::test]
fn the_percentage_waits_for_70(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, view_with_usage(69_000, 100_000));
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-context").is_some());
    assert!(bounds_of(cx, window, "quick-context-percent").is_none());
}

// quick-panel / Unknown context size: nothing shown.
#[gpui_kit::test]
fn no_ring_without_a_context_size(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    show_view(cx, &rig, &panel, long_view("chat", 1, "idle"));
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-header").is_some());
    assert!(bounds_of(cx, window, "quick-context").is_none());
    assert!(bounds_of(cx, window, "quick-context-percent").is_none());
}

// quick-panel / Mode for the next quick chat: Tab in the bar moves the mode
// of that chat only; the start page's launch mode is left alone.
#[gpui_kit::test]
fn mode_for_the_next_quick_chat_only(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    cx.update(|cx| {
        rig.store
            .update(cx, |s, _| s.prefs.launch_mode = "build".into())
    });
    let (window, panel) = rig.open_panel(cx);
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-mode:Build").is_some());

    press(cx, window, "tab");
    settle(cx, window);
    assert_eq!(
        panel
            .read_with(cx, |p, _| p.picks_for_test().mode)
            .as_deref(),
        Some("plan")
    );
    assert!(bounds_of(cx, window, "quick-mode:Plan").is_some());
    press(cx, window, "shift-tab");
    settle(cx, window);
    assert!(bounds_of(cx, window, "quick-mode:Build").is_some());
    press(cx, window, "tab");

    type_draft(cx, window, &panel, "plan the work");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.submit(window, cx))
    })
    .unwrap();
    let sent = rig.pump(cx);
    assert!(sent.iter().any(|r| r["method"] == "session/prompt"));

    let started = pane_entity(cx, &panel).expect("the quick chat started");
    assert_eq!(
        started.read_with(cx, |o, _| o.view.mode.clone()).as_deref(),
        Some("plan"),
        "the new session starts in Plan"
    );
    assert_eq!(
        rig.store.read_with(cx, |s, _| s.prefs.launch_mode.clone()),
        "build",
        "the start page's launch mode is untouched"
    );

    // The choice was for that chat: the next one starts from the default.
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.new_chat(window, cx))
    })
    .unwrap();
    settle(cx, window);
    assert_eq!(panel.read_with(cx, |p, _| p.picks_for_test().mode), None);
    assert!(bounds_of(cx, window, "quick-mode:Build").is_some());
}

#[gpui_kit::test]
fn the_folder_is_the_panels_choice_and_the_quick_chat_starts_in_it(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_folder_chooser(window, cx))
    })
    .unwrap();
    assert!(panel.read_with(cx, |p, _| p.folder_open_for_test()));
    settle(cx, window);
    let sheet = bounds_of(cx, window, "quick-folders").expect("the chooser");
    let bar = bounds_of(cx, window, "quick-bar").expect("the bar");
    assert!(bottom(sheet) <= top(bar) + 0.5, "above the bar");

    // Esc closes the chooser and not the panel.
    press(cx, window, "escape");
    assert!(!closed(cx, &rig));
    assert!(!panel.read_with(cx, |p, _| p.folder_open_for_test()));

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.choose_folder("/srv/app/", window, cx))
    })
    .unwrap();
    assert_eq!(
        panel
            .read_with(cx, |p, _| p.picks_for_test().cwd)
            .as_deref(),
        Some("/srv/app")
    );
    type_draft(cx, window, &panel, "hello");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.submit(window, cx))
    })
    .unwrap();
    let sent = rig.pump(cx);
    let new = sent
        .iter()
        .find(|r| r["method"] == "session/new")
        .expect("a session was created");
    assert_eq!(new["params"]["cwd"], "/srv/app");
}

#[gpui_kit::test]
fn a_quick_chat_without_a_folder_asks_for_one(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| rig.store.update(cx, |s, _| s.prefs.cwd.clear()));
    let (window, panel) = rig.open_panel(cx);
    type_draft(cx, window, &panel, "hello");
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.submit(window, cx))
    })
    .unwrap();
    let sent = rig.pump(cx);
    assert!(sent.iter().all(|r| r["method"] != "session/new"));
    assert!(panel.read_with(cx, |p, _| p.folder_open_for_test()));
    assert_eq!(draft(cx, &panel), "hello");
}

#[gpui_kit::test]
fn the_reasoning_shortcut_changes_only_the_panels_next_quick_chat(cx: &mut TestAppContext) {
    bind_keys(cx);
    let rig = rig(cx);
    let before = rig
        .store
        .read_with(cx, |s, _| s.prefs.reasoning().to_owned());
    let (window, panel) = rig.open_panel(cx);
    let t = secondary(cx, "t");
    press(cx, window, &t);
    let picked = panel.read_with(cx, |p, _| p.picks_for_test().reasoning);
    assert!(picked.is_some_and(|r| r != before), "{before}");
    assert_eq!(
        rig.store
            .read_with(cx, |s, _| s.prefs.reasoning().to_owned()),
        before,
        "the start page's default is untouched"
    );
}

#[gpui_kit::test]
fn the_main_modifier_and_enter_starts_the_chat_in_the_main_window(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    type_draft(cx, window, &panel, "show me");
    let enter = secondary(cx, "enter");
    press(cx, window, &enter);
    rig.pump(cx);
    assert_eq!(
        rig.main_key(cx).map(|k| k.session.to_string()).as_deref(),
        Some("q1")
    );
    assert!(closed(cx, &rig));
}

// quick-panel / The bar's controls: One size for the labels. The labels lie
// on one line under the input, their text starting on its leading edge.
#[gpui_kit::test]
fn the_bars_labels_lie_on_one_line_under_the_input(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    finish_opening(cx, window);
    let input = bounds_of(cx, window, "quick-composer").expect("the input");
    let centre_y = |b: gpui_kit::Bounds<gpui_kit::Pixels>| {
        f32::from(b.origin.y) + f32::from(b.size.height) / 2.
    };
    let ids = [
        "quick-folder",
        "quick-mode",
        "quick-model",
        "quick-reasoning",
    ];
    let labels: Vec<_> = ids
        .iter()
        .map(|id| bounds_of(cx, window, id).unwrap_or_else(|| panic!("{id} is drawn")))
        .collect();
    for (id, b) in ids.iter().zip(&labels) {
        assert!(
            (centre_y(*b) - centre_y(labels[0])).abs() <= 1.,
            "{id} shares the folder's centre line"
        );
        assert!(b.top() >= input.bottom(), "{id} is under the input");
        assert!(
            (f32::from(b.size.height) - f32::from(labels[0].size.height)).abs() <= 0.5,
            "{id} is as high as the folder"
        );
        assert!(
            b.size.height < input.size.height,
            "{id} is smaller than the input"
        );
    }
    // Left to right, without overlap.
    for pair in labels.windows(2) {
        assert!(pair[0].right() <= pair[1].left(), "{pair:?}");
    }
    // The leftmost label (the agent's name when there is a choice) has its
    // text where the prompt's text starts, which is the caret of the empty
    // input, not the input's own edge: the kit insets its lines.
    let first = bounds_of(cx, window, "quick-agent").unwrap_or(labels[0]);
    let text_left = f32::from(first.left()) + 6.;
    let caret = caret_bounds(cx, window, &panel)
        .into_iter()
        .find(|c| c.top() >= input.top() && c.bottom() <= input.bottom())
        .expect("the input's caret");
    let caret_left = f32::from(caret.left());
    assert!(
        (text_left - caret_left).abs() <= 1.,
        "labels' text at {text_left}, the prompt's caret at {caret_left}"
    );
    assert!(
        (caret_left - f32::from(input.left()) - super::panel::BAR_TEXT_INSET).abs() <= 1.,
        "the kit's inset is still {} px",
        super::panel::BAR_TEXT_INSET
    );
    // The session picker is at the row's trailing end.
    let picker = bounds_of(cx, window, "quick-picker-toggle").expect("picker button");
    assert!(picker.left() >= labels[3].right());
    assert!((centre_y(picker) - centre_y(labels[0])).abs() <= 1.);
}

// quick-panel / Long prompt in the bar: a long draft keeps the input's full
// width and the labels on one line under it; the bar grows.
#[gpui_kit::test]
fn a_long_prompt_keeps_the_full_width(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    finish_opening(cx, window);
    let ids = [
        "quick-folder",
        "quick-mode",
        "quick-model",
        "quick-reasoning",
    ];
    let at_rest = bounds_of(cx, window, "quick-composer").expect("the input");
    let bar_at_rest = bounds_of(cx, window, "quick-bar").expect("the bar");

    type_draft(cx, window, &panel, "one\ntwo\nthree\nfour");
    settle(cx, window);
    settle(cx, window);
    let input = bounds_of(cx, window, "quick-composer").expect("the input");
    assert_eq!(input.size.width, at_rest.size.width, "the full width");
    assert!(input.size.height > at_rest.size.height, "the input grows");
    let bar = bounds_of(cx, window, "quick-bar").expect("the bar");
    assert!(bar.size.height > bar_at_rest.size.height, "the bar grows");
    let tops: Vec<f32> = ids
        .iter()
        .map(|id| {
            let b = bounds_of(cx, window, id).unwrap_or_else(|| panic!("{id} is drawn"));
            assert!(b.top() >= input.bottom(), "{id} is under the input");
            f32::from(b.origin.y)
        })
        .collect();
    assert!(
        tops.iter().all(|t| (t - tops[0]).abs() <= 1.),
        "the labels stay on one line: {tops:?}"
    );

    // Cleared, the bar is the resting bar again.
    type_draft(cx, window, &panel, "");
    settle(cx, window);
    settle(cx, window);
    let bar = bounds_of(cx, window, "quick-bar").expect("the bar");
    assert_eq!(bar.size.height, bar_at_rest.size.height);
}

// quick-panel / The bar's controls: Mode and reasoning as labels. Each is a
// button; the model picker stays closed.
#[gpui_kit::test]
fn the_mode_and_reasoning_labels_cycle(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| {
        rig.store.update(cx, |s, _| {
            s.prefs.launch_mode = "build".into();
            s.prefs.launch_reasoning = "auto".into();
        })
    });
    let (window, panel) = rig.open_panel(cx);
    finish_opening(cx, window);
    let picks = |cx: &mut TestAppContext| panel.read_with(cx, |p, _| p.picks_for_test());
    let at = |cx: &mut TestAppContext, id: &'static str| {
        centre(bounds_of(cx, window, id).unwrap_or_else(|| panic!("{id} is drawn")))
    };

    // The mode's label is as wide as its widest name, so the labels after it
    // stay where they are whatever mode is shown.
    let model_at = |cx: &mut TestAppContext| bounds_of(cx, window, "quick-model").unwrap();
    let (model, mode) = (model_at(cx), bounds_of(cx, window, "quick-mode").unwrap());
    for (next, name) in [("plan", "Plan"), ("review", "Review"), ("build", "Build")] {
        let tag = at(cx, "quick-mode");
        press_at(cx, window, tag);
        settle(cx, window);
        assert_eq!(picks(cx).mode.as_deref(), Some(next));
        let shown = Box::leak(format!("quick-mode:{name}").into_boxed_str());
        assert!(bounds_of(cx, window, shown).is_some(), "{name} is shown");
        assert_eq!(
            bounds_of(cx, window, "quick-mode").unwrap(),
            mode,
            "the mode label keeps its size at {name}"
        );
        assert_eq!(model_at(cx), model, "the model label stays put at {name}");
    }
    let tag = at(cx, "quick-mode");
    press_at_with(cx, window, tag, gpui_kit::Modifiers::shift());
    assert_eq!(picks(cx).mode.as_deref(), Some("review"));

    let tag = at(cx, "quick-reasoning");
    press_at(cx, window, tag);
    assert_eq!(picks(cx).reasoning.as_deref(), Some("low"));

    assert!(
        !panel.read_with(cx, |p, cx| p.models_for_test().read(cx).is_open()),
        "the model picker stays closed"
    );
    assert_eq!(drags(cx, &panel), 0, "a label is not a drag area");
}

// ---- progress and the card grid in the session picker ----------------------------

/// `session` with a todo plan of entries in these states.
fn with_plan(mut session: SessionSummary, statuses: &[&str]) -> SessionSummary {
    session.plans = vec![ListedPlan {
        id: "querymt-todos".into(),
        entries: statuses
            .iter()
            .enumerate()
            .map(|(i, status)| PlanEntry {
                content: format!("todo {i}"),
                status: (*status).into(),
            })
            .collect(),
    }];
    session
}

/// Bounds of an element whose debug selector is built at run time.
fn bounds_dyn(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    selector: String,
) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
    bounds_of(cx, window, Box::leak(selector.into_boxed_str()))
}

fn width(b: gpui_kit::Bounds<gpui_kit::Pixels>) -> f32 {
    f32::from(b.size.width)
}

fn top(b: gpui_kit::Bounds<gpui_kit::Pixels>) -> f32 {
    f32::from(b.origin.y)
}

fn bottom(b: gpui_kit::Bounds<gpui_kit::Pixels>) -> f32 {
    f32::from(b.origin.y + b.size.height)
}

fn set_layout(cx: &mut TestAppContext, rig: &Rig, layout: PickerLayout) {
    cx.update(|cx| {
        rig.store
            .update(cx, |s, cx| s.set_quick_picker_layout(layout, cx))
    });
}

/// Open the picker the way its button does and let the panel settle.
fn show_picker(cx: &mut TestAppContext, window: AnyWindowHandle, panel: &Entity<QuickPanel>) {
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.toggle_picker_for_test(window, cx))
    })
    .unwrap();
    settle(cx, window);
}

fn row_ids(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> Vec<String> {
    panel.read_with(cx, |p, cx| {
        p.picker_rows(cx).into_iter().map(|r| r.id).collect()
    })
}

// quick-panel / Picker rows show progress
#[gpui_kit::test]
fn picker_rows_carry_the_listed_plan(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            with_plan(
                summary("a", "Planned", "idle", "2"),
                &["completed", "completed", "pending", "pending"],
            ),
            summary("b", "Plain", "idle", "1"),
        ],
    );
    let rows = rig.store.read_with(cx, |s, cx| picker_rows(s, "", cx));
    let digest = rows[0].plan.as_ref().expect("a has progress");
    assert_eq!((digest.done, digest.total), (2, 4));
    assert!(rows[1].plan.is_none());
}

// quick-panel / Picker rows show progress: the bar of each state
#[gpui_kit::test]
fn a_bar_has_a_section_per_todo_and_says_whether_the_session_is_live(cx: &mut TestAppContext) {
    use super::panel::{BarPulse, BarSection, live_bar};
    let rig = rig(cx);
    let half = ["completed", "completed", "in_progress", "pending"];
    rig.summaries(
        cx,
        vec![
            with_plan(summary("run", "Run", "running", "6"), &half),
            summary("spin", "Spin", "running", "5"),
            with_plan(summary("ask", "Ask", "running", "4"), &half),
            with_plan(summary("left", "Left", "idle", "3"), &half),
            summary("none", "None", "idle", "2"),
            with_plan(
                summary("between", "Between", "running", "1"),
                &["completed", "pending"],
            ),
        ],
    );
    cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.request_for_test(
                "test-agent",
                1,
                "session/request_permission",
                json!({
                    "sessionId": "ask",
                    "toolCall": { "toolCallId": "t1", "title": "Run ls", "kind": "execute" },
                    "options": [{ "optionId": "o", "name": "Allow", "kind": "allow_once" }],
                }),
                cx,
            )
        })
    });
    let rows = rig.store.read_with(cx, |s, cx| picker_rows(s, "", cx));
    let bar = |id: &str| live_bar(rows.iter().find(|r| r.id == id).expect(id));
    use BarSection::{Active, Done, Pending};
    let sections = vec![Done, Done, Active, Pending];

    // Running: one section per todo, the one being worked on breathing.
    let run = bar("run").expect("a bar");
    assert_eq!(run.sections, sections);
    assert_eq!(run.pulse, BarPulse::Section(2));
    assert!(!run.idle && !run.waiting && !run.indeterminate && !run.finished);
    // Running without todos: nothing to count, a segment slides.
    let spin = bar("spin").expect("a bar");
    assert!(spin.indeterminate && !spin.idle);
    // Running between todos: the whole bar breathes.
    assert_eq!(bar("between").expect("a bar").pulse, BarPulse::Whole);
    // Waiting for the user: the same sections, still.
    let ask = bar("ask").expect("a bar");
    assert_eq!(ask.sections, sections);
    assert!(ask.waiting && !ask.idle && !ask.indeterminate);
    assert_eq!(ask.pulse, BarPulse::None);
    // Idle: the todos as they were left, still and grey.
    let left = bar("left").expect("a bar");
    assert_eq!(left.sections, sections);
    assert!(left.idle && !left.waiting);
    assert_eq!(left.pulse, BarPulse::None);
    // Idle without todos: no bar.
    assert!(bar("none").is_none());

    // Only the waiting session has a chip, and it asks for the user.
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let ids = row_ids(cx, &panel);
    for (ix, id) in ids.iter().enumerate() {
        let chip = bounds_dyn(cx, window, format!("quick-pick-tone:{ix}:Needs input"));
        assert_eq!(chip.is_some(), id == "ask", "{id}");
    }
    // The chip does not move the columns: the waiting row's bar and time sit
    // where a running row's do.
    let ix = |id: &str| ids.iter().position(|i| i == id).expect(id);
    let (ask, run) = (ix("ask"), ix("run"));
    let left = |cx: &mut TestAppContext, sel: String| {
        f32::from(bounds_dyn(cx, window, sel).expect("drawn").left())
    };
    let (a, r) = (
        left(cx, format!("quick-pick-bar:{ask}")),
        left(cx, format!("quick-pick-bar:{run}")),
    );
    assert!((a - r).abs() < 0.5, "bars: {a} vs {r}");
    let (a, r) = (
        left(cx, format!("quick-pick-time:{ask}:")),
        left(cx, format!("quick-pick-time:{run}:")),
    );
    assert!((a - r).abs() < 0.5, "times: {a} vs {r}");
}

// quick-panel / One highlight in the picker: a pointer that creeps in steps
// under a pixel still moves the highlight once it has gone a pixel.
#[gpui_kit::test]
fn slow_pointer_movement_adds_up_to_a_move(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("a", "Session A", "idle", "3"),
            summary("b", "Session B", "idle", "2"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let cursor = |cx: &mut TestAppContext| panel.read_with(cx, |p, _| p.picker_cursor_for_test());
    let row0 = bounds_dyn(cx, window, "quick-pick-row:0".into()).expect("row");
    let row1 = bounds_dyn(cx, window, "quick-pick-row:1".into()).expect("row");
    let move_to = |cx: &mut TestAppContext, at| {
        VisualTestContext::from_window(window, cx).simulate_mouse_move(
            at,
            None,
            gpui_kit::Modifiers::none(),
        );
    };
    // The first event places the pointer over the cursor's own row.
    let mut at = centre(row0);
    move_to(cx, at);
    assert_eq!(cursor(cx), 0);
    // Straight down into row 1 in steps of 0.4 px: each is under the
    // threshold, together they are over it. Measured from the last
    // position that counted, the highlight follows; measured from the last
    // event, it would never move.
    let step = gpui_kit::px(0.4);
    let target = centre(row1).y;
    let mut moves = 0;
    while cursor(cx) == 0 && at.y < target && moves < 400 {
        at = gpui_kit::point(at.x, at.y + step);
        move_to(cx, at);
        moves += 1;
    }
    assert_eq!(cursor(cx), 1, "the small steps added up");
}

// quick-panel / The bar stays readable and calm: Reduced motion
#[gpui_kit::test]
fn with_motion_off_the_bars_are_still_and_ask_for_no_frames(cx: &mut TestAppContext) {
    use super::panel::bar_motion;
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![with_plan(
            summary("run", "Run", "running", "2"),
            &["completed", "in_progress"],
        )],
    );
    let live = rig.store.read_with(cx, |s, cx| picker_rows(s, "", cx));
    rig.summaries(
        cx,
        vec![with_plan(
            summary("left", "Left", "idle", "2"),
            &["completed", "pending"],
        )],
    );
    let idle = rig.store.read_with(cx, |s, cx| picker_rows(s, "", cx));

    // Moving: a live bar breathes and wants the next frame; idle ones do not.
    let (a, b) = (bar_motion(&live, true, 0.), bar_motion(&live, true, 700.));
    assert!(a.animate && a.alpha != b.alpha && a.slide != b.slide);
    assert!(
        !bar_motion(&idle, true, 0.).animate,
        "nothing moves, no frames"
    );
    // Motion off: the same at every moment, and no frames.
    let still: Vec<_> = [0., 350., 700., 1100.]
        .map(|t| bar_motion(&live, false, t))
        .to_vec();
    assert!(
        still.iter().all(|m| !m.animate && *m == still[0]),
        "{still:?}"
    );
    assert!(
        still[0].alpha < 1. && still[0].alpha > 0.4,
        "set apart, not out"
    );
}

// quick-panel / Picker rows show progress: the pulse breathes between 40% and
// full, never out, and is the same for every row at one moment.
#[test]
fn the_pulse_breathes_and_never_goes_out() {
    use super::panel::pulse_alpha;
    let alphas: Vec<f32> = (0..300).map(|i| pulse_alpha(f64::from(i) * 10.)).collect();
    let (lo, hi) = alphas
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), a| (lo.min(*a), hi.max(*a)));
    assert!(
        (lo - 0.4).abs() < 0.01 && (hi - 1.).abs() < 0.01,
        "{lo} {hi}"
    );
}

// quick-panel / Picker rows show progress: Live session, Session without todos,
// Idle session with todos
#[gpui_kit::test]
fn list_rows_put_the_bar_beside_the_title_and_say_what_a_live_session_does(
    cx: &mut TestAppContext,
) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            with_plan(
                summary("half", "Half", "running", "2025-01-05T11:00:00Z"),
                &["completed", "completed", "in_progress", "pending"],
            ),
            summary("none", "None", "idle", "2025-01-05T10:00:00Z"),
            with_plan(
                summary("left", "Left", "idle", "2025-01-05T09:00:00Z"),
                &["completed", "in_progress", "pending"],
            ),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    assert_eq!(row_ids(cx, &panel), ["half", "none", "left"]);

    let at = |cx: &mut TestAppContext, sel: String| bounds_dyn(cx, window, sel);
    let row = |cx: &mut TestAppContext, ix: usize| at(cx, format!("quick-pick-row:{ix}")).unwrap();
    let title =
        |cx: &mut TestAppContext, ix: usize| at(cx, format!("quick-pick-title:{ix}")).unwrap();
    let bar = |cx: &mut TestAppContext, ix: usize| at(cx, format!("quick-pick-bar:{ix}"));

    // Live: a bar after the title, inside the row; the folder and the todo
    // being worked on under the title; no time, it is now.
    let (r, t, b) = (row(cx, 0), title(cx, 0), bar(cx, 0).expect("a bar"));
    assert!(
        f32::from(t.right()) <= f32::from(b.left()) + 0.5,
        "{t:?} {b:?}"
    );
    assert!(
        b.top() >= r.top() && b.bottom() <= r.bottom(),
        "{b:?} in {r:?}"
    );
    assert!(width(b) > 0.);
    assert!(at(cx, "quick-pick-sub:0:work/half".into()).is_some());
    assert!(at(cx, "quick-pick-doing:0:todo 2".into()).is_some());
    assert!(at(cx, "quick-pick-time:0:".into()).is_some());

    // Idle without todos: no bar, but its place is kept, so the times form a
    // column; the title shares the column of the others, the row is no taller.
    assert!(bar(cx, 1).is_none());
    let time = |cx: &mut TestAppContext, ix: usize| {
        at(cx, format!("quick-pick-time:{ix}:Jan 5")).expect("the time")
    };
    let (t1, t2) = (time(cx, 1), time(cx, 2));
    assert!((f32::from(t1.left()) - f32::from(t2.left())).abs() < 0.5);
    assert!((f32::from(title(cx, 1).left()) - f32::from(t.left())).abs() < 0.5);
    assert!((f32::from(row(cx, 0).size.height) - f32::from(row(cx, 1).size.height)).abs() < 0.5);

    // Idle with todos: its bar, as it was left, and when; not what it was
    // doing, it is not doing it now.
    assert!(bar(cx, 2).is_some());
    assert!(at(cx, "quick-pick-doing:2:todo 1".into()).is_none());
}

// quick-panel / Picker rows show progress: Worktree session
#[gpui_kit::test]
fn a_worktree_row_names_its_project_and_only_a_branch_a_person_named(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let worktree = |id: &str, branch: &str| {
        let mut s = summary(id, id, "idle", "2");
        s.cwd = format!("/home/me/.qmt/worktrees/ab12/{id}");
        s.worktree = Some(crate::store::Worktree {
            branch: Some(branch.into()),
            source_cwd: Some("/home/me/qmt/querymt".into()),
            ..Default::default()
        });
        s
    };
    rig.summaries(
        cx,
        vec![
            worktree("gen", "qmt/workspace/0f3c"),
            worktree("named", "fix-login"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    assert_eq!(row_ids(cx, &panel), ["gen", "named"]);
    let has = |cx: &mut TestAppContext, sel: &str| bounds_dyn(cx, window, sel.into()).is_some();
    // The project, not the worktree's hashed folder.
    assert!(has(cx, "quick-pick-sub:0:qmt/querymt"));
    assert!(has(cx, "quick-pick-sub:1:qmt/querymt"));
    // Both have the branch mark; only the branch a person named is named.
    assert!(has(cx, "quick-pick-worktree:0") && has(cx, "quick-pick-worktree:1"));
    assert!(!has(cx, "quick-pick-branch:0:qmt/workspace/0f3c"));
    assert!(has(cx, "quick-pick-branch:1:fix-login"));
}

// quick-panel / Rows keep their columns: No session has a bar
#[gpui_kit::test]
fn a_list_without_bars_keeps_no_place_for_one(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(cx, vec![summary("a", "A", "idle", "2")]);
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let title = |cx: &mut TestAppContext| {
        width(bounds_dyn(cx, window, "quick-pick-title:0".into()).expect("the title"))
    };
    assert!(bounds_dyn(cx, window, "quick-pick-bar:0".into()).is_none());
    let without = title(cx);

    // The session gets todos: a bar, and the title gives up its place.
    cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.notify_for_test(
                "test-agent",
                "_querymt/session/plansChanged",
                json!({ "session_id": "a", "plans": [{ "type": "items", "planId": "querymt-todos",
                    "entries": [{ "content": "x", "priority": "medium", "status": "pending" }] }] }),
                cx,
            )
        })
    });
    settle(cx, window);
    assert!(bounds_dyn(cx, window, "quick-pick-bar:0".into()).is_some());
    let with = title(cx);
    assert!(without - with >= 56. - 0.5, "{without} then {with}");
}

// quick-panel / Picker rows show progress: the list has one highlight, the
// row the keyboard is on, which the pointer moves too.
#[gpui_kit::test]
fn the_pointer_moves_the_list_cursor_so_there_is_one_highlight(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("a", "Session A", "idle", "3"),
            summary("b", "Session B", "idle", "2"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let cursor = |cx: &mut TestAppContext| panel.read_with(cx, |p, _| p.picker_cursor_for_test());
    assert_eq!(cursor(cx), 0);
    let at = centre(bounds_dyn(cx, window, "quick-pick-row:1".into()).expect("row"));
    VisualTestContext::from_window(window, cx).simulate_mouse_move(
        at,
        None,
        gpui_kit::Modifiers::none(),
    );
    cx.run_until_parked();
    settle(cx, window);
    assert_eq!(cursor(cx), 1, "the pointer's row is the keyboard's row");
}

// quick-panel / Picker rows show progress: Progress moves while the picker is open
#[gpui_kit::test]
fn progress_moves_while_the_picker_is_open(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            with_plan(
                summary("a", "A", "running", "2"),
                &["completed", "in_progress", "pending"],
            ),
            summary("b", "B", "idle", "1"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let doing = |cx: &mut TestAppContext, todo: &str| {
        bounds_dyn(cx, window, format!("quick-pick-doing:0:{todo}")).is_some()
    };
    let done = |cx: &mut TestAppContext| {
        panel.read_with(cx, |p, cx| {
            p.picker_rows(cx)[0]
                .plan
                .as_ref()
                .map(|d| (d.done, d.total))
        })
    };
    assert!(doing(cx, "todo 1"));
    assert_eq!(done(cx), Some((1, 3)));
    let drawn = |cx: &mut TestAppContext, i: usize, state: &str| {
        bounds_dyn(cx, window, format!("quick-pick-bar-section:0:{i}:{state}")).is_some()
    };
    assert!(drawn(cx, 0, "done") && drawn(cx, 1, "active") && drawn(cx, 2, "pending"));
    let before = row_ids(cx, &panel);

    let entries: Vec<Value> = ["completed", "completed", "in_progress"]
        .iter()
        .enumerate()
        .map(|(i, s)| json!({ "content": format!("todo {i}"), "priority": "medium", "status": s }))
        .collect();
    cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.notify_for_test(
                "test-agent",
                "_querymt/session/plansChanged",
                json!({ "session_id": "a", "plans": [
                    { "type": "items", "planId": "querymt-todos", "entries": entries }
                ] }),
                cx,
            )
        })
    });
    settle(cx, window);
    assert!(doing(cx, "todo 2") && !doing(cx, "todo 1"));
    assert_eq!(done(cx), Some((2, 3)), "the store's digest moved");
    // The bar drawn moved with it: two sections done, the third in progress.
    let drawn = |cx: &mut TestAppContext, i: usize, state: &str| {
        bounds_dyn(cx, window, format!("quick-pick-bar-section:0:{i}:{state}")).is_some()
    };
    assert!(drawn(cx, 0, "done") && drawn(cx, 1, "done") && drawn(cx, 2, "active"));
    assert!(!drawn(cx, 1, "active") && !drawn(cx, 1, "pending"));
    assert_eq!(row_ids(cx, &panel), before, "no reordering");
    assert!(
        panel.read_with(cx, |p, _| p.picker_open_for_test()),
        "still open"
    );
}

// quick-panel / Picker layout switch: Layout remembered
#[gpui_kit::test]
fn the_layout_is_saved_and_the_picker_opens_in_it(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(cx, vec![summary("a", "A", "idle", "1")]);
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    assert!(
        bounds_dyn(cx, window, "quick-pick-row:0".into()).is_some(),
        "list by default"
    );

    // The toggle sits next to the search input and switches the layout.
    let toggle = bounds_of(cx, window, "quick-picker-layout").expect("a toggle");
    let search = bounds_of(cx, window, "quick-picker-search").expect("the search row");
    assert!(top(toggle) >= top(search) && bottom(toggle) <= bottom(search) + 1.);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.toggle_picker_layout_for_test(window, cx))
    })
    .unwrap();
    settle(cx, window);
    assert!(bounds_dyn(cx, window, "quick-pick-card:0".into()).is_some());
    assert!(bounds_dyn(cx, window, "quick-pick-row:0".into()).is_none());

    // Close it, "restart": the settings as they were saved are loaded again.
    cx.update(|cx| panel.update(cx, |p, cx| p.set_picker_for_test(false, cx)));
    let saved = rig
        .store
        .read_with(cx, |s, _| serde_json::to_string(&s.prefs).unwrap());
    cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.set_quick_picker_layout(PickerLayout::List, cx);
            s.prefs = serde_json::from_str::<Prefs>(&saved).unwrap();
        })
    });
    cx.update(|cx| panel.update(cx, |p, cx| p.set_picker_for_test(true, cx)));
    settle(cx, window);
    assert!(
        bounds_dyn(cx, window, "quick-pick-card:0".into()).is_some(),
        "opens in the grid"
    );
}

// quick-panel / Picker card grid: Card of a session with todos, Card of a
// session without todos
#[gpui_kit::test]
fn a_card_shows_the_todos_a_session_is_at(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            with_plan(
                summary("a", "Planned", "running", "2"),
                &[
                    "completed",
                    "completed",
                    "in_progress",
                    "pending",
                    "pending",
                    "pending",
                ],
            ),
            summary("b", "Plain", "idle", "1"),
        ],
    );
    set_layout(cx, &rig, PickerLayout::Grid);
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let has = |cx: &mut TestAppContext, sel: &str| bounds_dyn(cx, window, sel.into()).is_some();

    // A bar across the card, under the title.
    let card = bounds_dyn(cx, window, "quick-pick-card:0".into()).expect("a card");
    let bar = bounds_dyn(cx, window, "quick-pick-bar:0".into()).expect("a bar");
    assert!(
        width(bar) > width(card) / 2. && top(bar) > top(card),
        "{bar:?} in {card:?}"
    );
    // The one in progress, the next two, and what is left.
    assert!(has(cx, "quick-card-line:0:todo 2"));
    assert!(has(cx, "quick-card-line:0:todo 3"));
    assert!(has(cx, "quick-card-line:0:todo 4"));
    assert!(!has(cx, "quick-card-line:0:todo 5"));
    assert!(
        !has(cx, "quick-card-line:0:todo 0"),
        "completed ones are not listed"
    );
    assert!(has(cx, "quick-card-more:0:1"), "+1 more");

    // The plain session: a card, with no bar and no todo lines.
    assert!(has(cx, "quick-pick-card:1"));
    assert!(!has(cx, "quick-pick-bar:1"));
    assert!(!has(cx, "quick-card-more:1:1"));
}

// quick-panel / Picker card grid: Same order in both layouts
#[gpui_kit::test]
fn both_layouts_list_the_same_sessions_in_the_same_order(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let mut sessions: Vec<SessionSummary> = (0..6)
        .map(|i| {
            summary(
                &format!("idle{i}"),
                "Idle",
                "idle",
                &format!("2026-05-0{}", i + 1),
            )
        })
        .collect();
    sessions.push(summary("run", "Running", "running", "2026-04-01"));
    sessions.push(summary("wait", "Waiting", "running", "2026-03-01"));
    rig.summaries(cx, sessions);
    cx.update(|cx| {
        rig.store.update(cx, |s, cx| {
            s.request_for_test(
                "test-agent",
                1,
                "session/request_permission",
                json!({
                    "sessionId": "wait",
                    "toolCall": { "toolCallId": "t1", "title": "Run ls", "kind": "execute" },
                    "options": [{ "optionId": "o", "name": "Allow", "kind": "allow_once" }],
                }),
                cx,
            )
        })
    });
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let expected = [
        "wait", "run", "idle5", "idle4", "idle3", "idle2", "idle1", "idle0",
    ];
    assert_eq!(row_ids(cx, &panel), expected);
    // Rows run down the picker.
    let tops: Vec<f32> = (0..8)
        .map(|i| top(bounds_dyn(cx, window, format!("quick-pick-row:{i}")).unwrap()))
        .collect();
    assert!(tops.windows(2).all(|w| w[0] < w[1]), "{tops:?}");

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.toggle_picker_layout_for_test(window, cx))
    })
    .unwrap();
    settle(cx, window);
    assert_eq!(row_ids(cx, &panel), expected);
    // Cards fill two columns, left to right and then down.
    let places: Vec<(f32, f32)> = (0..8)
        .map(|i| {
            let b = bounds_dyn(cx, window, format!("quick-pick-card:{i}")).unwrap();
            (top(b), f32::from(b.origin.x))
        })
        .collect();
    assert!(places.windows(2).all(|w| w[0] < w[1]), "{places:?}");
    assert_eq!(
        places.iter().filter(|p| p.0 == places[0].0).count(),
        2,
        "two columns: {places:?} in {:?}",
        bounds_of(cx, window, "quick-picker-cards")
    );
}

// quick-panel / Picker card grid: Pick from the grid
#[gpui_kit::test]
fn clicking_a_card_picks_its_session(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            summary("a", "Session A", "running", "2"),
            summary("b", "Session B", "idle", "1"),
        ],
    );
    set_layout(cx, &rig, PickerLayout::Grid);
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    let card = bounds_dyn(cx, window, "quick-pick-card:1".into()).expect("a card");
    let mut vcx = VisualTestContext::from_window(window, cx);
    // The test platform never reports a resize to GPUI: it lays out and hit-tests
    // inside the bar's 64 px, so the click lands near the card's top edge.
    let at = card.origin + gpui_kit::point(gpui_kit::px(10.), gpui_kit::px(3.));
    vcx.simulate_mouse_move(at, None, gpui_kit::Modifiers::none());
    vcx.simulate_mouse_down(at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::none());
    vcx.simulate_mouse_up(at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(pane_key(cx, &panel).as_deref(), Some("b"));
}

fn eight_idle_sessions(rig: &Rig, cx: &mut TestAppContext) {
    let sessions = (0..8)
        .map(|i| {
            with_plan(
                summary(
                    &format!("s{i}"),
                    &format!("Session {i}"),
                    "idle",
                    &i.to_string(),
                ),
                &["completed", "in_progress", "pending", "pending", "pending"],
            )
        })
        .collect();
    rig.summaries(cx, sessions);
}

// quick-panel / Grid picker fits the panel: Grid in a compact panel
#[gpui_kit::test]
fn the_grid_of_eight_cards_fits_without_scrolling(cx: &mut TestAppContext) {
    let rig = rig(cx);
    eight_idle_sessions(&rig, cx);
    set_layout(cx, &rig, PickerLayout::Grid);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1200.));
    settle(cx, window);
    assert_eq!(panel_height(cx, &panel), BAR_HEIGHT);
    show_picker(cx, window, &panel);
    finish_opening(cx, window);
    let height = panel_height(cx, &panel);
    let cards = bounds_of(cx, window, "quick-picker-cards").expect("the cards");
    let last = bounds_dyn(cx, window, "quick-pick-card:7".into()).expect("eight cards");
    assert!(
        bottom(last) <= bottom(cards) + 0.5,
        "every card is visible: {last:?} in {cards:?}"
    );
    assert!(bottom(cards) <= height, "inside the panel");
}

// quick-panel / Grid picker fits the panel: Grid on a small display
#[gpui_kit::test]
fn on_a_small_display_the_grid_stops_at_the_limit_and_scrolls(cx: &mut TestAppContext) {
    // Eight cards lay out to about 650 px with the bar: the old estimate
    // (746) said an 800 px display scrolled, the measured height says it fits.
    for (display, scrolls) in [(1200., false), (800., false), (600., true)] {
        let rig = rig(cx);
        eight_idle_sessions(&rig, cx);
        set_layout(cx, &rig, PickerLayout::Grid);
        let (window, panel) = rig.open_panel(cx);
        panel.update(cx, |p, _| p.set_display_height_for_test(display));
        settle(cx, window);
        show_picker(cx, window, &panel);
        finish_opening(cx, window);
        let height = panel_height(cx, &panel);
        // Never past what the display leaves the panel, and short of the
        // 746 px eight cards want.
        assert!(height <= display * 0.9 + 0.5, "{height} on {display}");
        assert!(height < picker_height(PickerLayout::Grid, 8) || !scrolls);
        let search = bounds_of(cx, window, "quick-picker-search").expect("the search row");
        let toggle = bounds_of(cx, window, "quick-picker-layout").expect("the toggle");
        let cards = bounds_of(cx, window, "quick-picker-cards").expect("the cards");
        assert!(
            bottom(search) <= top(cards) + 0.5,
            "the search row stays above the cards"
        );
        assert!(bottom(toggle) <= height, "the toggle stays in view");
        assert!(bottom(cards) <= height, "the cards stay inside the panel");
        let last = bounds_dyn(cx, window, "quick-pick-card:7".into()).expect("eight cards");
        assert_eq!(
            bottom(last) > bottom(cards) + 0.5,
            scrolls,
            "{display}: {last:?} in {cards:?}"
        );
    }
}

// ---- the session list opens in place (design D17) ----------------------------

fn list_sessions(rig: &Rig, cx: &mut TestAppContext, n: usize) {
    let sessions = (0..n)
        .map(|i| {
            summary(
                &format!("s{i}"),
                &format!("Session {i}"),
                "idle",
                &i.to_string(),
            )
        })
        .collect();
    rig.summaries(cx, sessions);
    set_layout(cx, rig, PickerLayout::List);
}

// quick-panel / List in place of the conversation
#[gpui_kit::test]
fn the_session_list_replaces_the_conversation(cx: &mut TestAppContext) {
    let rig = rig(cx);
    list_sessions(&rig, cx, 4);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    show_view(cx, &rig, &panel, long_view("long", 20, "idle"));
    settle(cx, window);
    assert!(
        bounds_of(cx, window, "composer-highlight").is_some(),
        "the conversation is shown"
    );
    assert!(bounds_of(cx, window, "quick-picker").is_none());

    show_picker(cx, window, &panel);
    // No transcript row is drawn under the list, and the list is a plain
    // column, not a floating card laid over it.
    assert!(
        bounds_of(cx, window, "composer-highlight").is_none(),
        "the conversation gave its place to the list"
    );
    let header = bounds_of(cx, window, "quick-header").expect("the header stays");
    let list = bounds_of(cx, window, "quick-picker").expect("the list");
    let search = bounds_of(cx, window, "quick-picker-search").expect("the search row");
    assert!(
        top(list) >= bottom(header) - 0.5,
        "{list:?} under {header:?}"
    );
    assert!(top(search) >= bottom(header) - 0.5);
    for i in 0..4 {
        let row = bounds_dyn(cx, window, format!("quick-pick-row:{i}")).expect("a row");
        assert!(top(row) >= bottom(search) - 0.5, "row {i} under the search");
        assert!(bottom(row) <= bottom(list) + 0.5, "row {i} inside the list");
    }
}

// quick-panel / List in place of the conversation: back where it was
#[gpui_kit::test]
fn closing_the_list_restores_the_scroll_position(cx: &mut TestAppContext) {
    let rig = rig(cx);
    list_sessions(&rig, cx, 4);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    show_view(cx, &rig, &panel, long_view("long", 80, "idle"));
    settle(cx, window);
    let pane = panel.read_with(cx, |p, _| p.pane_for_test().clone());
    pane.update(cx, |pane, _| pane.scroll_to_item_for_test(12));
    settle(cx, window);
    let before = pane.read_with(cx, |pane, _| pane.scroll_top_for_test());
    assert_eq!(before.item_ix, 12);

    show_picker(cx, window, &panel);
    show_picker(cx, window, &panel);
    assert!(bounds_of(cx, window, "quick-picker").is_none(), "closed");
    assert!(bounds_of(cx, window, "composer-highlight").is_some());
    let after = pane.read_with(cx, |pane, _| pane.scroll_top_for_test());
    assert_eq!(after, before);
}

fn set_rem(cx: &mut TestAppContext, window: AnyWindowHandle, rem: f32) {
    cx.update_window(window, |_, window, _| window.set_rem_size(rem))
        .unwrap();
}

// quick-panel / Every listed session is visible
#[gpui_kit::test]
fn the_last_listed_session_is_inside_the_window(cx: &mut TestAppContext) {
    for rem in [16., 20.] {
        let rig = rig(cx);
        list_sessions(&rig, cx, 8);
        let (window, panel) = rig.open_panel(cx);
        panel.update(cx, |p, _| p.set_display_height_for_test(1200.));
        set_rem(cx, window, rem);
        settle(cx, window);
        show_picker(cx, window, &panel);
        finish_opening(cx, window);
        let height = panel_height(cx, &panel);
        let last = bounds_dyn(cx, window, "quick-pick-row:7".into()).expect("eight rows");
        assert!(
            bottom(last) <= height + 0.5,
            "rem {rem}: {last:?} in a window {height} tall"
        );
        // Not by luck of a generous estimate: the window is as tall as the
        // list needs.
        let list = bounds_of(cx, window, "quick-picker-list").expect("the rows' box");
        assert!(bottom(last) >= bottom(list) - 16.5, "rem {rem}: no slack");
    }
}

// quick-panel / Every listed session is visible: a short display
#[gpui_kit::test]
fn on_a_short_display_the_rows_scroll_under_the_search_row(cx: &mut TestAppContext) {
    let rig = rig(cx);
    list_sessions(&rig, cx, 8);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(400.));
    settle(cx, window);
    show_picker(cx, window, &panel);
    finish_opening(cx, window);
    let height = panel_height(cx, &panel);
    assert!(height <= 400. * 0.9 + 0.5, "{height}");
    let search = bounds_of(cx, window, "quick-picker-search").expect("the search row");
    let rows = bounds_of(cx, window, "quick-picker-list").expect("the rows' box");
    assert!(
        bottom(search) <= top(rows) + 0.5,
        "the search row stays above"
    );
    assert!(bottom(search) <= height, "and in view");
    let last = bounds_dyn(cx, window, "quick-pick-row:7".into()).expect("eight rows");
    assert!(
        bottom(last) > bottom(rows) + 0.5,
        "the rows scroll: {last:?} {rows:?}"
    );
}

// ---- typed text in the panel's colours (gpui-kit 0.11) -----------------------

/// The colour typed text is painted in, in the field `state`: mark `text` as
/// IME composition, whose underline is drawn in the typed-text colour, and
/// read that underline back.
fn typed_colour<S: gpui_kit::EntityInputHandler>(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    state: &Entity<S>,
) -> Option<gpui_kit::Hsla> {
    cx.update_window(window, |_, window, cx| {
        state.update(cx, |s, cx| {
            s.replace_and_mark_text_in_range(None, "typed", None, window, cx)
        })
    })
    .unwrap();
    settle(cx, window);
    let underlines = cx
        .update_window(window, |_, window, _| window.painted_underlines())
        .unwrap();
    let colour = underlines.last().map(|u| u.color);
    cx.update_window(window, |_, window, cx| {
        state.update(cx, |s, cx| s.unmark_text(window, cx))
    })
    .unwrap();
    colour
}

// quick-panel / Typed text in a light app theme: the text itself. The kit's
// input paints in the app theme's foreground unless told otherwise.
#[gpui_kit::test]
fn typed_text_is_the_panels_text_colour_in_a_light_app_theme(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    cx.update(|cx| crate::theme::apply(crate::theme::Appearance::builtin(false), cx));
    let (window, panel) = rig.open_panel(cx);
    // Until the opening animation ends the panel is drawn see-through.
    finish_opening(cx, window);
    let hud = panel.read_with(cx, |p, cx| p.hud_for_test(cx));
    let app = cx.update(|cx| crate::theme::palette(cx).text);
    assert_ne!(hud.text, app, "the test needs the two to differ");

    let composer = panel.read_with(cx, |p, cx| {
        p.pane_for_test().read(cx).prompt_input().clone()
    });
    assert_eq!(
        typed_colour(cx, window, &composer),
        Some(hud.text),
        "composer"
    );

    show_picker(cx, window, &panel);
    let query = panel.read_with(cx, |p, _| p.picker_query_for_test().clone());
    assert_eq!(
        typed_colour(cx, window, &query),
        Some(hud.text),
        "session search"
    );
    show_picker(cx, window, &panel);

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
    settle(cx, window);
    let query = panel.read_with(cx, |p, cx| {
        p.models_for_test().read(cx).query_for_test().clone()
    });
    assert_eq!(
        typed_colour(cx, window, &query),
        Some(hud.text),
        "model search"
    );
    press(cx, window, "escape");

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_folder_chooser(window, cx))
    })
    .unwrap();
    settle(cx, window);
    let field = panel.read_with(cx, |p, cx| p.folder_input_for_test(cx));
    assert_eq!(
        typed_colour(cx, window, &field),
        Some(hud.text),
        "folder field"
    );
}

// quick-panel / Every folder suggestion is visible
#[gpui_kit::test]
fn every_folder_suggestion_is_inside_the_window(cx: &mut TestAppContext) {
    let rig = rig(cx);
    // Six sessions, six workspaces (`/work/s0` …) to suggest.
    list_sessions(&rig, cx, 6);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_folder_chooser(window, cx))
    })
    .unwrap();
    let field = panel.read_with(cx, |p, cx| p.folder_input_for_test(cx));
    cx.update_window(window, |_, window, cx| {
        field.update(cx, |f, cx| {
            gpui_kit::EntityInputHandler::replace_text_in_range(f, None, "/work/s", window, cx)
        })
    })
    .unwrap();
    settle(cx, window);
    finish_opening(cx, window);
    let height = panel_height(cx, &panel);
    let last = bounds_dyn(cx, window, "ws-suggestion:5".into()).expect("six suggestions");
    assert!(
        bottom(last) <= height + 0.5,
        "{last:?} in a window {height} tall"
    );
    // Laid out in the chooser, not over the panel.
    let sheet = bounds_of(cx, window, "quick-folders").expect("the chooser");
    assert!(bottom(last) <= bottom(sheet) + 0.5, "inside the chooser");
}

/// Open the chooser, type `text` into its field and press `keys` in turn.
fn type_in_folder_chooser(
    cx: &mut TestAppContext,
    rig: &Rig,
    text: &str,
    keys: &[&str],
) -> Option<String> {
    list_sessions(rig, cx, 6);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_folder_chooser(window, cx))
    })
    .unwrap();
    let field = panel.read_with(cx, |p, cx| p.folder_input_for_test(cx));
    // The chooser opens on the current folder: replace it.
    cx.update_window(window, |_, window, cx| {
        field.update(cx, |f, cx| f.set_value("", window, cx));
        field.update(cx, |f, cx| {
            gpui_kit::EntityInputHandler::replace_text_in_range(f, None, text, window, cx)
        })
    })
    .unwrap();
    settle(cx, window);
    for key in keys {
        press(cx, window, key);
    }
    panel.read_with(cx, |p, _| p.picks_for_test().cwd)
}

// quick-panel / Enter takes the typed folder: a suggestion that only matches
// what is typed is not used until an arrow key moves to it.
#[gpui_kit::test]
fn enter_in_the_folder_field_takes_what_is_typed(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let cwd = type_in_folder_chooser(cx, &rig, "/work/s", &["enter"]);
    assert_eq!(cwd.as_deref(), Some("/work/s"), "not /work/s0");
}

#[gpui_kit::test]
fn enter_takes_a_typed_folder_next_to_a_longer_recent_one(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let cwd = type_in_folder_chooser(cx, &rig, "/work/s1", &["enter"]);
    assert_eq!(cwd.as_deref(), Some("/work/s1"));
}

#[gpui_kit::test]
fn enter_after_an_arrow_key_takes_the_suggestion(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let cwd = type_in_folder_chooser(cx, &rig, "/work/s", &["down", "enter"]);
    let cwd = cwd.expect("a folder was chosen");
    assert!(
        cwd.starts_with("/work/s") && cwd != "/work/s",
        "the highlighted suggestion: {cwd}"
    );
}

// ---- moving the panel (design D18) -------------------------------------------

mod placement {
    use gpui_kit::{Bounds, point, px, size};

    use crate::quick::panel::{placed, position_of};
    use crate::store::QuickPosition;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<gpui_kit::Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    const WIDTH: f32 = crate::quick::panel::WIDTH;

    #[test]
    fn a_place_left_on_a_display_comes_back_on_it() {
        // A second display to the right of the first, under a menu bar.
        let display = rect(1440., 0., 1920., 1080.);
        let visible = rect(1440., 25., 1920., 1055.);
        let left_at = rect(1700., 300., WIDTH, 200.);
        let position = position_of(left_at, display);
        assert_eq!(
            position,
            QuickPosition {
                left: 260,
                top: 300,
                bottom: 500,
            }
        );
        // macOS keeps the bottom edge: a compact bar comes back on it.
        let bar = placed(display, visible, position, 64., true);
        assert_eq!(bar, rect(1700., 436., WIDTH, 64.));
        // Windows and X11 keep the top edge.
        let bar = placed(display, visible, position, 64., false);
        assert_eq!(bar, rect(1700., 300., WIDTH, 64.));
    }

    #[test]
    fn a_place_off_a_smaller_display_is_moved_inside_it() {
        // Left near the bottom right of a big display, which is now smaller.
        let position = QuickPosition {
            left: 2400,
            top: 1300,
            bottom: 1400,
        };
        let display = rect(0., 0., 1440., 900.);
        let visible = rect(0., 25., 1440., 875.);
        for bottom_anchored in [true, false] {
            let b = placed(display, visible, position, 100., bottom_anchored);
            assert_eq!(b.right(), visible.right(), "inside on the right");
            assert_eq!(b.bottom(), visible.bottom(), "inside at the bottom");
        }
        // And never above the visible area (under the menu bar).
        let high = QuickPosition {
            left: -50,
            top: -40,
            bottom: 10,
        };
        let b = placed(display, visible, high, 200., true);
        assert_eq!((b.left(), b.top()), (visible.left(), visible.top()));
    }
}

/// The primary display's key and bounds, and the panel's bounds.
fn display_and_panel(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
) -> (
    String,
    gpui_kit::Bounds<gpui_kit::Pixels>,
    gpui_kit::Bounds<gpui_kit::Pixels>,
) {
    cx.update_window(window, |_, window, cx| {
        let display = cx.primary_display().expect("a display");
        (
            super::panel::display_key(&*display),
            display.bounds(),
            window.bounds(),
        )
    })
    .unwrap()
}

// quick-panel / Reopen where it was left
#[gpui_kit::test]
fn the_panel_reopens_where_it_was_left_on_its_display(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, _) = rig.open_panel(cx);
    let (key, display, centred) = display_and_panel(cx, window);
    rig.hide(cx);
    assert!(closed(cx, &rig));

    // Left 120 px left of and 80 px above where it opened.
    let moved = gpui_kit::Bounds::new(
        centred.origin - gpui_kit::point(gpui_kit::px(120.), gpui_kit::px(80.)),
        centred.size,
    );
    let position = super::panel::position_of(moved, display);
    cx.update(|cx| {
        rig.store
            .update(cx, |s, _| s.remember_quick_position(key.clone(), position))
    });
    let (window, _) = rig.open_panel(cx);
    let (_, _, bounds) = display_and_panel(cx, window);
    assert_eq!(bounds.origin, moved.origin, "where it was left");
    rig.hide(cx);

    // A place left on another display is not used here.
    cx.update(|cx| {
        rig.store.update(cx, |s, _| {
            s.prefs.quick.positions.clear();
            s.remember_quick_position("another-display".into(), position);
        })
    });
    let (window, _) = rig.open_panel(cx);
    let (_, _, bounds) = display_and_panel(cx, window);
    assert_eq!(bounds.origin, centred.origin, "centred as before");
}

// quick-panel / Drag the panel: whenever the window moves or grows, where it
// is on its display is remembered.
#[gpui_kit::test]
fn the_panel_remembers_where_it_is(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, _) = rig.open_panel(cx);
    let (key, display, bounds) = display_and_panel(cx, window);
    // The platform reports a new frame (the test window can only resize).
    let taller = gpui_kit::size(bounds.size.width, bounds.size.height + gpui_kit::px(40.));
    cx.simulate_window_resize(window, taller);
    cx.run_until_parked();
    let (_, _, now) = display_and_panel(cx, window);
    assert_eq!(now.size, taller);
    let stored = cx.update(|cx| rig.store.read(cx).prefs.quick.positions.get(&key).copied());
    assert_eq!(stored, Some(super::panel::position_of(now, display)));
}

fn press_at(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    at: gpui_kit::Point<gpui_kit::Pixels>,
) {
    press_at_with(cx, window, at, gpui_kit::Modifiers::none());
}

fn press_at_with(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    at: gpui_kit::Point<gpui_kit::Pixels>,
    modifiers: gpui_kit::Modifiers,
) {
    let mut vcx = VisualTestContext::from_window(window, cx);
    vcx.simulate_mouse_move(at, None, modifiers);
    vcx.simulate_mouse_down(at, gpui_kit::MouseButton::Left, modifiers);
    vcx.simulate_mouse_up(at, gpui_kit::MouseButton::Left, modifiers);
    cx.run_until_parked();
}

fn drags(cx: &mut TestAppContext, panel: &Entity<QuickPanel>) -> usize {
    panel.read_with(cx, |p, _| p.drags_for_test())
}

fn centre(b: gpui_kit::Bounds<gpui_kit::Pixels>) -> gpui_kit::Point<gpui_kit::Pixels> {
    b.center()
}

// quick-panel / Typing is not dragging; Drag the panel
#[gpui_kit::test]
fn the_bar_drags_around_its_controls_only(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    finish_opening(cx, window);
    let bar = bounds_of(cx, window, "quick-bar").expect("the bar");
    let composer = bounds_of(cx, window, "quick-composer").expect("the composer");

    press_at(cx, window, centre(composer));
    assert_eq!(drags(cx, &panel), 0, "pressing in the text does not drag");
    for id in [
        "quick-folder",
        "quick-mode",
        "quick-model",
        "quick-reasoning",
        "quick-picker-toggle",
    ] {
        let label = bounds_of(cx, window, id).unwrap_or_else(|| panic!("{id} is drawn"));
        press_at(cx, window, centre(label));
        assert_eq!(drags(cx, &panel), 0, "{id} is a button");
    }

    // The bar's padding, left of the text.
    let edge = gpui_kit::point(bar.left() + gpui_kit::px(4.), centre(bar).y);
    assert!(edge.x < composer.left());
    press_at(cx, window, edge);
    assert_eq!(drags(cx, &panel), 1, "the bar around its controls drags");
}

// quick-panel / Drag the panel: the header
#[gpui_kit::test]
fn the_header_drags_outside_its_buttons(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    show_view(cx, &rig, &panel, long_view("long", 3, "idle"));
    settle(cx, window);
    finish_opening(cx, window);

    let free = bounds_of(cx, window, "quick-drag").expect("free header space");
    assert!(
        free.size.width > gpui_kit::px(100.),
        "room to grab: {free:?}"
    );
    press_at(cx, window, centre(free));
    assert_eq!(drags(cx, &panel), 1);

    // The title opens the session list and does not drag.
    let title = bounds_of(cx, window, "quick-title").expect("the title");
    press_at(cx, window, centre(title));
    assert_eq!(drags(cx, &panel), 1);
    assert!(panel.read_with(cx, |p, _| p.picker_open_for_test()));
}

/// Colours of the thin quads (carets) painted in the last frame.
fn caret_colours(cx: &mut TestAppContext, window: AnyWindowHandle) -> Vec<gpui_kit::Hsla> {
    cx.update_window(window, |_, window, _| {
        window
            .painted_quads()
            .iter()
            .filter(|q| q.bounds.size.width.0 <= 4. && q.bounds.size.height.0 >= 8.)
            .filter_map(|q| q.background.as_solid())
            .collect()
    })
    .unwrap()
}

/// Bounds of the panel's carets painted in the last frame (thin quads in its
/// accent).
fn caret_bounds(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    panel: &Entity<QuickPanel>,
) -> Vec<gpui_kit::Bounds<gpui_kit::Pixels>> {
    let accent = panel.read_with(cx, |p, cx| p.hud_for_test(cx)).accent;
    cx.update_window(window, |_, window, _| {
        // Painted quads are in device pixels.
        let scale = window.scale_factor();
        let logical = |v: gpui_kit::ScaledPixels| gpui_kit::px(v.0 / scale);
        window
            .painted_quads()
            .iter()
            .filter(|q| q.bounds.size.width.0 <= 4. && q.bounds.size.height.0 >= 8.)
            .filter(|q| q.background.as_solid() == Some(accent))
            .map(|q| {
                gpui_kit::Bounds::new(
                    gpui_kit::point(logical(q.bounds.origin.x), logical(q.bounds.origin.y)),
                    gpui_kit::size(logical(q.bounds.size.width), logical(q.bounds.size.height)),
                )
            })
            .collect()
    })
    .unwrap()
}

// quick-panel / Typed text in a light app theme: the caret, selection and
// placeholder are the panel's too. The caret is what a test can read back;
// the placeholder and the selection are set by the same style.
#[gpui_kit::test]
fn the_composers_caret_is_the_panels_in_a_light_app_theme(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| crate::theme::apply(crate::theme::Appearance::builtin(false), cx));
    let (window, panel) = rig.open_panel(cx);
    finish_opening(cx, window);
    let hud = panel.read_with(cx, |p, cx| p.hud_for_test(cx));
    let theme_caret = cx.update(|cx| gpui_kit::component::ActiveTheme::theme(cx).caret);
    assert_ne!(hud.accent, theme_caret, "the test needs the two to differ");
    let carets = caret_colours(cx, window);
    assert!(
        carets.contains(&hud.accent),
        "the panel's caret: {carets:?}"
    );
    assert!(
        !carets.contains(&theme_caret),
        "not the app theme's: {carets:?}"
    );

    // The composer of a conversation is the pane's.
    show_view(cx, &rig, &panel, long_view("long", 3, "idle"));
    cx.update_window(window, |_, window, cx| {
        let pane = panel.read(cx).pane_for_test().clone();
        pane.update(cx, |pane, cx| pane.focus_composer(window, cx));
    })
    .unwrap();
    settle(cx, window);
    let carets = caret_colours(cx, window);
    assert!(
        carets.contains(&hud.accent),
        "in a conversation: {carets:?}"
    );
    assert!(!carets.contains(&theme_caret), "{carets:?}");
}

// The style the panel's fields get.
#[gpui_kit::test]
fn the_panels_fields_use_its_palette(cx: &mut TestAppContext) {
    let _rig = rig(cx);
    let p = super::hud::hud_palette(&crate::theme::Palette::light(), true);
    let style = cx.update(|cx| crate::ui::kit::editor_style(&p, cx));
    assert_eq!(style.foreground, p.text);
    assert_eq!(style.muted_foreground, p.muted, "the placeholder");
    assert_eq!(style.caret, p.accent);
    assert_eq!(style.selection, p.accent.opacity(0.32));
}

// The panel keeps QueryMT Dark's code colours whatever theme the main window
// shows (desktop-themes / The quick panel keeps its own colours).
fn panel_pane_palette(
    cx: &mut TestAppContext,
    rig: &Rig,
) -> (crate::theme::Palette, crate::theme::Palette) {
    let (window, panel) = rig.open_panel(cx);
    finish_opening(cx, window);
    show_view(cx, rig, &panel, long_view("code", 1, "idle"));
    settle(cx, window);
    panel.read_with(cx, |p, cx| {
        (
            p.hud_for_test(cx),
            p.pane_for_test().read(cx).palette_for_test(cx),
        )
    })
}

fn assert_qmt_dark_code(
    cx: &mut TestAppContext,
    hud: crate::theme::Palette,
    pane: crate::theme::Palette,
) {
    use gpui_kit::component::highlighter::HighlightTheme;
    use std::sync::Arc;
    assert_eq!(pane, hud, "the pane paints with the panel's palette");
    assert_eq!(pane.theme, crate::theme::ThemeId::QMT_DARK);
    assert!(Arc::ptr_eq(
        &crate::theme::syntax(&pane),
        &HighlightTheme::default_dark()
    ));
    let style = crate::ui::view_style(&pane);
    assert!(
        Arc::ptr_eq(&style.highlight_theme, &HighlightTheme::default_dark()),
        "fences"
    );
    let app = cx.update(|cx| *crate::theme::palette(cx));
    assert!(
        !Arc::ptr_eq(&crate::theme::syntax(&app), &HighlightTheme::default_dark()),
        "the test needs the two to differ"
    );
}

#[gpui_kit::test]
fn panel_code_uses_qmt_dark_with_a_bundled_theme(cx: &mut TestAppContext) {
    use crate::theme::{Accent, Appearance, ThemeId};
    let rig = rig(cx);
    let theme = ThemeId::resolve("solarized-light", false);
    cx.update(|cx| {
        crate::theme::apply(
            Appearance {
                theme,
                accent: Accent::Theme,
            },
            cx,
        )
    });
    let (hud, pane) = panel_pane_palette(cx, &rig);
    assert_qmt_dark_code(cx, hud, pane);
    // Its surfaces are its own dark ones, not the theme's.
    let app = cx.update(|cx| *crate::theme::palette(cx));
    assert!(hud.dark && !app.dark);
    assert_ne!(hud.bg, app.bg);
    assert_ne!(hud.border, app.border);
}

#[gpui_kit::test]
fn panel_code_uses_qmt_dark_in_light_mode(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.update(|cx| crate::theme::apply(crate::theme::Appearance::builtin(false), cx));
    let (hud, pane) = panel_pane_palette(cx, &rig);
    assert_qmt_dark_code(cx, hud, pane);
}

#[gpui_kit::test]
fn panel_follows_the_theme_accent(cx: &mut TestAppContext) {
    use crate::theme::{Accent, Appearance, ThemeId};
    let rig = rig(cx);
    let theme = ThemeId::resolve("tokyo-night", true);
    cx.update(|cx| {
        crate::theme::apply(
            Appearance {
                theme,
                accent: Accent::Theme,
            },
            cx,
        )
    });
    let (window, panel) = rig.open_panel(cx);
    finish_opening(cx, window);
    let hud = panel.read_with(cx, |p, cx| p.hud_for_test(cx));
    let own = theme.accent();
    // Tokyo Night's primary, lightened for the dark glass.
    assert!((hud.accent.h - own.h).abs() < 1e-4 && (hud.accent.s - own.s).abs() < 1e-4);
    assert!(hud.accent.l >= own.l);
    let orange = crate::theme::Palette::dark().accent;
    assert!(
        (hud.accent.h - orange.h).abs() > 0.01,
        "not QueryMT's orange"
    );
}
