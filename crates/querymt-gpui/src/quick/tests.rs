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
        crate::theme::apply(true, "orange", cx);
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
    assert!(short <= 300., "a short chat needs little room, got {short}");

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
    // One line of large text at rest: a 26 px line, where the conversation's
    // composer has 20 px lines.
    let line = f32::from(input.size.height);
    assert!((40. ..48.).contains(&line), "one 26 px line, got {line}");
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
    // One 20 px line is shorter than one 26 px line, not two lines tall.
    assert!(composer <= bar, "{composer:?} vs {bar:?}");
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

#[gpui_kit::test]
fn open_model_picker_raises_the_height_floor(cx: &mut TestAppContext) {
    let rig = rig(cx);
    with_models(&rig, cx);
    let (window, panel) = rig.open_panel(cx);
    panel.update(cx, |p, _| p.set_display_height_for_test(1000.));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, cx| p.open_model_picker(window, cx))
    })
    .unwrap();
    settle(cx, window);
    assert!(panel_height(cx, &panel) >= 520.);
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
    assert!(panel_height(cx, &panel) >= 440., "room for the suggestions");

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

#[gpui_kit::test]
fn the_bars_controls_share_the_inputs_centre_line(cx: &mut TestAppContext) {
    let rig = rig(cx);
    let (window, _panel) = rig.open_panel(cx);
    settle(cx, window);
    let centre = |id: &'static str, cx: &mut TestAppContext| {
        let b = bounds_of(cx, window, id).unwrap_or_else(|| panic!("{id} is drawn"));
        f32::from(b.origin.y) + f32::from(b.size.height) / 2.
    };
    let input = centre("quick-composer", cx);
    for id in ["quick-model", "quick-folder", "quick-reasoning"] {
        let c = centre(id, cx);
        assert!((c - input).abs() <= 1., "{id}: {c} vs {input}");
    }
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

// quick-panel / Picker rows show progress: Half-done session, Session without
// todos, Finished plan
#[gpui_kit::test]
fn list_rows_fill_as_far_as_the_todos_are_done(cx: &mut TestAppContext) {
    let rig = rig(cx);
    rig.summaries(
        cx,
        vec![
            with_plan(
                summary("half", "Half", "idle", "3"),
                &["completed", "completed", "pending", "pending"],
            ),
            summary("none", "None", "idle", "2"),
            with_plan(summary("done", "Done", "idle", "1"), &["completed"; 4]),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    assert_eq!(row_ids(cx, &panel), ["half", "none", "done"]);

    let row = |cx: &mut TestAppContext, ix: usize| {
        bounds_dyn(cx, window, format!("quick-pick-row:{ix}")).expect("the row shows")
    };
    let fill = |cx: &mut TestAppContext, ix: usize| {
        bounds_dyn(cx, window, format!("quick-pick-fill:{ix}"))
    };
    // Half done: filled across half the row, with `2/4`.
    let (r, f) = (row(cx, 0), fill(cx, 0).expect("a fill"));
    assert!((width(f) - width(r) / 2.).abs() < 1.5, "{f:?} in {r:?}");
    assert!(bounds_dyn(cx, window, "quick-pick-count:0:2/4".into()).is_some());
    // No todos: no fill and no count.
    assert!(fill(cx, 1).is_none());
    assert!(bounds_dyn(cx, window, "quick-pick-count:1:0/0".into()).is_none());
    // Finished: the whole row, `4/4`.
    let (r, f) = (row(cx, 2), fill(cx, 2).expect("a fill"));
    assert!((width(f) - width(r)).abs() < 1.5, "{f:?} in {r:?}");
    assert!(bounds_dyn(cx, window, "quick-pick-count:2:4/4".into()).is_some());
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
                &["completed", "pending", "pending"],
            ),
            summary("b", "B", "idle", "1"),
        ],
    );
    let (window, panel) = rig.open_panel(cx);
    show_picker(cx, window, &panel);
    assert!(bounds_dyn(cx, window, "quick-pick-count:0:1/3".into()).is_some());
    let before = row_ids(cx, &panel);

    let entries: Vec<Value> = ["completed", "completed", "pending"]
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
    assert!(bounds_dyn(cx, window, "quick-pick-count:0:2/3".into()).is_some());
    assert!(bounds_dyn(cx, window, "quick-pick-count:0:1/3".into()).is_none());
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

    assert!(has(cx, "quick-card-bar:0:2/6"), "a bar with 2/6");
    let fill = bounds_dyn(cx, window, "quick-card-fill:0".into()).expect("filled");
    let bar = bounds_dyn(cx, window, "quick-card-bar:0:2/6".into()).unwrap();
    assert!(
        width(fill) > 0. && width(fill) < width(bar) / 2.,
        "a third of the track"
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
    assert!(!has(cx, "quick-card-fill:1"));
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
    let height = panel_height(cx, &panel);
    assert!(height >= picker_height(PickerLayout::Grid, 8), "{height}");
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
    for (display, scrolls) in [(800., false), (600., true)] {
        let rig = rig(cx);
        eight_idle_sessions(&rig, cx);
        set_layout(cx, &rig, PickerLayout::Grid);
        let (window, panel) = rig.open_panel(cx);
        panel.update(cx, |p, _| p.set_display_height_for_test(display));
        settle(cx, window);
        show_picker(cx, window, &panel);
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
