//! Several sessions open at once: routing of agent inputs to each, sharing,
//! lifetime and reconnects (spec `open-sessions`).

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{Value, json};

use super::{AgentsStore, ConnState};
use crate::rpc::RpcClient;
use crate::session::open::{Delivery, OpenSession, SessionKey, SessionRef};

fn target(session: &str) -> SessionRef {
    SessionRef {
        key: SessionKey::new("test-agent", session),
        cwd: "/w".into(),
        title: session.into(),
    }
}

/// A connected agent whose requests the test reads and answers.
struct Agent {
    store: Entity<AgentsStore>,
    rpc: Rc<RpcClient>,
    requests: async_channel::Receiver<String>,
}

impl Agent {
    fn new(cx: &mut TestAppContext) -> Self {
        let store = cx.new(|_| crate::store::automations::schedule_ui_fixture());
        let (rpc, requests) = cx.update(|cx| store.update(cx, |s, _| s.rpc_for_test("test-agent")));
        Self {
            store,
            rpc,
            requests,
        }
    }

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

    fn open(&self, cx: &mut TestAppContext, session: &str) -> Entity<OpenSession> {
        cx.update(|cx| self.store.update(cx, |s, cx| s.open(target(session), cx)))
    }

    /// Answer the `session/load` requests sent so far.
    fn finish_loads(&self, cx: &mut TestAppContext) -> usize {
        let loads: Vec<Value> = self
            .sent(cx)
            .into_iter()
            .filter(|r| r["method"] == "session/load")
            .collect();
        for load in &loads {
            self.reply(cx, load, json!({}));
        }
        loads.len()
    }

    fn notify(&self, cx: &mut TestAppContext, method: &str, params: Value) {
        cx.update(|cx| {
            self.store.update(cx, |s, cx| {
                s.notify_for_test("test-agent", method, params, cx)
            })
        });
    }

    /// Let the sessions' flush timers fire.
    fn flush(&self, cx: &mut TestAppContext) {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
    }
}

fn chunk(session: &str, text: &str) -> Value {
    json!({
        "sessionId": session,
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "messageId": format!("m-{session}"),
            "content": { "type": "text", "text": text },
        },
    })
}

/// Everything the transcript of `session` shows, as text.
fn transcript(cx: &mut TestAppContext, session: &Entity<OpenSession>) -> String {
    session.read_with(cx, |o, _| {
        o.view
            .items
            .iter()
            .map(|item| format!("{:?}", item.kind))
            .collect()
    })
}

#[gpui_kit::test]
fn two_open_sessions_receive_only_their_updates(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    let a = agent.open(cx, "a");
    let b = agent.open(cx, "b");
    assert_eq!(agent.finish_loads(cx), 2);

    agent.notify(cx, "session/update", chunk("a", "text for a"));
    agent.notify(cx, "session/update", chunk("b", "text for b"));
    agent.notify(cx, "session/update", chunk("a", " and more for a"));
    agent.flush(cx);

    let (ta, tb) = (transcript(cx, &a), transcript(cx, &b));
    assert!(
        ta.contains("text for a") && ta.contains("and more for a"),
        "{ta}"
    );
    assert!(!ta.contains("for b"), "{ta}");
    assert!(tb.contains("text for b"), "{tb}");
    assert!(!tb.contains("for a"), "{tb}");
}

fn command_names(cx: &mut TestAppContext, session: &Entity<OpenSession>) -> Vec<String> {
    session.read_with(cx, |o, _| {
        o.view
            .slash_commands
            .iter()
            .map(|c| c.name.clone())
            .collect()
    })
}

/// Commands reach an open session directly; for a session nobody holds they
/// are kept until it opens, and survive its load (design D3).
#[gpui_kit::test]
fn commands_go_to_the_open_session_or_wait_for_it(cx: &mut TestAppContext) {
    use super::early_commands::commands_update;
    let agent = Agent::new(cx);
    let a = agent.open(cx, "a");
    agent.finish_loads(cx);

    agent.notify(cx, "session/update", commands_update("a", &["compact"]));
    agent.notify(cx, "session/update", commands_update("b", &["worktree"]));
    agent.flush(cx);
    assert_eq!(command_names(cx, &a), ["compact"]);
    let held = agent.store.read_with(cx, |s, _| s.early_commands_len());
    assert_eq!(held, 1, "only the unopened session's update is held");

    let b = agent.open(cx, "b");
    assert_eq!(agent.finish_loads(cx), 1);
    agent.flush(cx);
    assert_eq!(command_names(cx, &b), ["worktree"]);
    let held = agent.store.read_with(cx, |s, _| s.early_commands_len());
    assert_eq!(held, 0, "the view took the entry");
}

#[gpui_kit::test]
fn opening_an_open_session_sends_no_second_load(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    let first = agent.open(cx, "a");
    let second = agent.open(cx, "a");
    assert_eq!(first.entity_id(), second.entity_id());
    assert_eq!(agent.finish_loads(cx), 1);
    // Still one load after both surfaces showed it.
    let third = agent.open(cx, "a");
    assert_eq!(first.entity_id(), third.entity_id());
    assert_eq!(agent.finish_loads(cx), 0);
}

#[gpui_kit::test]
fn update_for_unshown_session_changes_no_transcript(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    let a = agent.open(cx, "a");
    agent.finish_loads(cx);
    cx.update(|cx| {
        agent.store.update(cx, |s, _| {
            s.runtime_mut_for_test("test-agent")
                .sessions
                .push(super::SessionSummary {
                    agent_id: "test-agent".into(),
                    id: "unshown".into(),
                    phase: "idle".into(),
                    ..Default::default()
                });
        })
    });

    agent.notify(cx, "session/update", chunk("unshown", "nobody sees this"));
    agent.notify(
        cx,
        "_querymt/session/runtimeChanged",
        json!({ "session_id": "unshown", "runtime": { "phase": "running" } }),
    );
    agent.flush(cx);

    assert!(transcript(cx, &a).is_empty());
    // The session list still follows the session's run state.
    let phase = agent.store.read_with(cx, |s, _| {
        s.session_summary("test-agent", "unshown")
            .map(|s| s.phase.clone())
    });
    assert_eq!(phase.as_deref(), Some("running"));
}

#[gpui_kit::test]
fn late_load_after_drop_is_ignored(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    let kept = agent.open(cx, "kept");
    let dropped = agent.open(cx, "gone");
    let loads = agent.sent(cx);
    assert_eq!(loads.len(), 2);

    // The surface stops showing "gone" before its response arrives.
    drop(dropped);
    cx.run_until_parked();
    for load in &loads {
        agent.reply(cx, load, json!({}));
    }
    agent.flush(cx);

    let key = SessionKey::new("test-agent", "gone");
    cx.update(|cx| {
        agent.store.update(cx, |s, cx| {
            assert!(s.with_session(&key, cx, |_| ()).is_none(), "closed");
            assert!(
                s.with_session(&SessionKey::new("test-agent", "kept"), cx, |_| ())
                    .is_some()
            );
        })
    });
    assert!(kept.read_with(cx, |o, _| !o.view.loading && o.view.error.is_none()));
}

#[gpui_kit::test]
fn reconnect_rebinds_every_open_session(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    let a = agent.open(cx, "a");
    let b = agent.open(cx, "b");
    agent.finish_loads(cx);
    let linked = |cx: &mut TestAppContext, s: &Entity<OpenSession>| {
        s.read_with(cx, |o, _| (o.link().rpc.is_some(), o.link().connected))
    };
    assert_eq!(
        (linked(cx, &a), linked(cx, &b)),
        ((true, true), (true, true))
    );

    cx.update(|cx| {
        agent
            .store
            .update(cx, |s, cx| s.disconnect("test-agent", cx))
    });
    assert_eq!(
        (linked(cx, &a), linked(cx, &b)),
        ((false, false), (false, false))
    );

    // A new connection: both sessions get it and load again on it.
    let (rpc, requests) = cx.update(|cx| {
        agent.store.update(cx, |s, cx| {
            let linked = s.rpc_for_test("test-agent");
            s.runtime_mut_for_test("test-agent").state = ConnState::Connected;
            s.rebind_sessions("test-agent", true, cx);
            linked
        })
    });
    cx.run_until_parked();
    let loads: Vec<Value> = std::iter::from_fn(|| requests.try_recv().ok())
        .map(|text| serde_json::from_str(&text).unwrap())
        .collect();
    let mut ids: Vec<&str> = loads
        .iter()
        .filter(|r| r["method"] == "session/load")
        .filter_map(|r| r["params"]["sessionId"].as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["a", "b"]);
    assert_eq!(
        (linked(cx, &a), linked(cx, &b)),
        ((true, true), (true, true))
    );
    drop(rpc);
}

#[gpui_kit::test]
fn nested_update_does_not_double_lease(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    let a = agent.open(cx, "a");
    let b = agent.open(cx, "b");
    agent.finish_loads(cx);

    // Prompt both, one after the other.
    for session in [&a, &b] {
        cx.update(|cx| {
            agent.store.update(cx, |s, cx| {
                s.send_prompt(session, "hello".into(), Vec::new(), Delivery::Steer, cx);
            })
        });
    }
    let prompts: Vec<Value> = agent
        .sent(cx)
        .into_iter()
        .filter(|r| r["method"] == "session/prompt")
        .collect();
    assert_eq!(prompts.len(), 2);
    // Completing a turn makes the session emit an event the store handles.
    for prompt in &prompts {
        agent.reply(cx, prompt, json!({ "stopReason": "end_turn" }));
    }
    assert!(a.read_with(cx, |o, _| !o.view.prompting));
    assert!(b.read_with(cx, |o, _| !o.view.prompting));

    // And a reconnect updates every session from the store.
    cx.update(|cx| {
        agent
            .store
            .update(cx, |s, cx| s.rebind_sessions("test-agent", true, cx))
    });
    cx.run_until_parked();
    let reloads = agent
        .sent(cx)
        .into_iter()
        .filter(|r| r["method"] == "session/load")
        .count();
    assert_eq!(reloads, 2);
}

fn launch_request(agent: &str, cwd: &str, prompt: &str) -> super::launch::LaunchRequest {
    super::launch::LaunchRequest {
        agent_id: agent.into(),
        cwd: cwd.into(),
        prompt: prompt.into(),
        attachments: Vec::new(),
        placement: super::launch::Placement::local(),
        model: None,
        reasoning: None,
    }
}

type Launch = gpui_kit::Task<Result<Entity<OpenSession>, super::launch::LaunchError>>;

fn launch(cx: &mut TestAppContext, agent: &Agent, request: super::launch::LaunchRequest) -> Launch {
    cx.update(|cx| agent.store.update(cx, |s, cx| s.launch(request, cx)))
}

/// Answer `session/new` with an id named after its workspace, and return the
/// `session/prompt` requests seen.
fn serve_launches(cx: &mut TestAppContext, agent: &Agent) -> Vec<Value> {
    let mut prompts = Vec::new();
    loop {
        let sent = agent.sent(cx);
        if sent.is_empty() {
            break;
        }
        for request in sent {
            match request["method"].as_str() {
                Some("session/new") => {
                    let cwd = request["params"]["cwd"]
                        .as_str()
                        .unwrap()
                        .trim_start_matches('/');
                    agent.reply(cx, &request, json!({ "sessionId": format!("s-{cwd}") }));
                }
                Some("session/prompt") => prompts.push(request),
                _ => agent.reply(cx, &request, json!({})),
            }
        }
    }
    prompts
}

#[gpui_kit::test]
fn two_launches_both_complete(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    // The second starts before the first finished: neither cancels the other.
    let first = launch(cx, &agent, launch_request("test-agent", "/a", "one"));
    let second = launch(cx, &agent, launch_request("test-agent", "/b", "two"));
    let prompts = serve_launches(cx, &agent);
    assert_eq!(prompts.len(), 2);

    let first = futures::FutureExt::now_or_never(first)
        .expect("first finished")
        .expect("first session");
    let second = futures::FutureExt::now_or_never(second)
        .expect("second finished")
        .expect("second session");
    assert_eq!(first.read_with(cx, |o, _| o.key.session.to_string()), "s-a");
    assert_eq!(
        second.read_with(cx, |o, _| o.key.session.to_string()),
        "s-b"
    );
    // Launching shows nothing anywhere: each caller shows its own.
    // Both are open sessions, routed independently.
    agent.notify(cx, "session/update", chunk("s-a", "only for a"));
    agent.flush(cx);
    assert!(transcript(cx, &first).contains("only for a"));
    assert!(!transcript(cx, &second).contains("only for a"));
}

#[gpui_kit::test]
fn launch_error_reaches_only_its_caller(cx: &mut TestAppContext) {
    let agent = Agent::new(cx);
    let good = launch(cx, &agent, launch_request("test-agent", "/a", "fine"));
    // An agent without a connection fails its own launch at once.
    let bad = launch(cx, &agent, launch_request("no-such-agent", "/b", "lost"));
    assert!(matches!(
        futures::FutureExt::now_or_never(bad),
        Some(Err(super::launch::LaunchError::NotConnected))
    ));

    serve_launches(cx, &agent);
    let good = futures::FutureExt::now_or_never(good)
        .expect("finished")
        .expect("the other launch is not affected");
    assert_eq!(good.read_with(cx, |o, _| o.key.session.to_string()), "s-a");
}
