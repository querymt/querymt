//! Plans (todo lists) of listed sessions: where the app learns a session's
//! progress without opening it (spec `session-plans`).
//!
//! `session/list` entries carry them in `_meta["querymt/plans"]`
//! (`SessionSummary::from_list_entry`); `querymt/session/plansChanged`
//! patches the listed summary in place; a session open in the app answers
//! from its own live plan.

use gpui_kit::*;
use serde_json::Value;

use super::{AgentsStore, SessionSummary};
use crate::session::{ListedPlan, PlanDigest};

impl AgentsStore {
    /// `querymt/session/plansChanged` from agent `id`: replace the plans of
    /// the named session. No `session/list` request; a session the app does
    /// not list is ignored (the next list refresh brings it).
    pub(super) fn on_plans_changed(&mut self, id: &str, params: &Value, cx: &mut Context<Self>) {
        let Some(session_id) = params
            .get("session_id")
            .or_else(|| params.get("sessionId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        let plans = ListedPlan::parse_all(params.get("plans"));
        let listed = rt
            .sessions
            .iter_mut()
            .chain(rt.children.values_mut().flat_map(|c| c.sessions.iter_mut()))
            .filter(|s| s.id == session_id);
        let mut changed = false;
        for summary in listed {
            if summary.plans != plans {
                summary.plans = plans.clone();
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Progress of `summary`'s session: its live plan when it is open in the
    /// app and has one, else the listed plans.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // read by the desktop-only quick panel
    pub fn plan_digest(&self, summary: &SessionSummary, cx: &App) -> Option<PlanDigest> {
        let key = crate::session::open::SessionKey::new(&summary.agent_id, &summary.id);
        let live = self
            .open_sessions
            .hub
            .get(&key)
            .and_then(WeakEntity::upgrade)
            .and_then(|open| PlanDigest::from_entries(&open.read(cx).view.plan));
        live.or_else(|| PlanDigest::from_plans(&summary.plans))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use serde_json::{Value, json};

    use super::{AgentsStore, SessionSummary};
    use crate::session::PlanDigest;
    use crate::session::open::{SessionKey, SessionRef};

    fn entry(session: &str, plans: Option<Value>) -> Value {
        let mut meta = json!({});
        if let Some(plans) = plans {
            meta["querymt/plans"] = plans;
        }
        json!({ "sessionId": session, "cwd": "/w", "title": session, "_meta": meta })
    }

    fn todos(statuses: &[&str]) -> Value {
        json!([{
            "type": "items",
            "planId": "querymt-todos",
            "entries": statuses.iter().enumerate()
                .map(|(i, s)| json!({
                    "content": format!("step {i}"), "priority": "medium", "status": s
                }))
                .collect::<Vec<_>>(),
        }])
    }

    fn digest(s: &SessionSummary) -> Option<(usize, usize)> {
        PlanDigest::from_plans(&s.plans).map(|d| (d.done, d.total))
    }

    fn listed(session: &str, statuses: &[&str]) -> SessionSummary {
        SessionSummary::from_list_entry("test-agent", &entry(session, Some(todos(statuses))))
    }

    #[test]
    fn plan_read_from_the_session_list() {
        let s = listed("a", &["completed", "pending", "pending"]);
        assert_eq!(digest(&s), Some((1, 3)));
        let names: Vec<&str> = s.plans[0]
            .entries
            .iter()
            .map(|e| e.content.as_str())
            .collect();
        assert_eq!(names, ["step 0", "step 1", "step 2"]);
    }

    #[test]
    fn agent_without_plans_in_its_list() {
        let s = SessionSummary::from_list_entry("test-agent", &entry("a", None));
        assert_eq!(s.id, "a");
        assert!(s.plans.is_empty());
        assert_eq!(digest(&s), None);
    }

    #[test]
    fn unknown_plan_type_is_ignored() {
        let plans = json!([{ "type": "markdown", "planId": "m", "markdown": "# plan" }]);
        let s = SessionSummary::from_list_entry("test-agent", &entry("a", Some(plans)));
        assert!(s.plans.is_empty());
    }

    #[test]
    fn malformed_plans_keep_the_session() {
        let s = SessionSummary::from_list_entry("test-agent", &entry("a", Some(json!("oops"))));
        assert_eq!(s.id, "a");
        assert!(s.plans.is_empty());
    }

    /// A store listing `sessions` for `test-agent`, with the requests the
    /// agent receives.
    fn store_with(
        cx: &mut TestAppContext,
        sessions: Vec<SessionSummary>,
    ) -> (Entity<AgentsStore>, Requests, u64) {
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.sessions = sessions;
        let generation = rt.generation;
        (cx.new(|_| store), Requests { rpc, requests }, generation)
    }

    /// What the agent receives, and its way to answer.
    struct Requests {
        rpc: std::rc::Rc<crate::rpc::RpcClient>,
        requests: async_channel::Receiver<String>,
    }

    impl Requests {
        fn try_recv(&self) -> Result<String, async_channel::TryRecvError> {
            self.requests.try_recv()
        }

        /// Answer the `session/load` requests sent so far.
        fn finish_loads(&self, cx: &mut TestAppContext) {
            cx.run_until_parked();
            while let Ok(text) = self.requests.try_recv() {
                let request: Value = serde_json::from_str(&text).unwrap();
                if request["method"] == "session/load" {
                    self.rpc
                        .handle(&json!({ "id": request["id"], "result": {} }).to_string());
                }
            }
            cx.run_until_parked();
        }
    }

    fn plans_changed(
        cx: &mut TestAppContext,
        store: &Entity<AgentsStore>,
        generation: u64,
        params: Value,
    ) {
        store.update(cx, |s, cx| {
            s.on_incoming(
                "test-agent",
                generation,
                crate::rpc::Incoming::Notification {
                    method: "_querymt/session/plansChanged".into(),
                    params,
                },
                cx,
            )
        });
    }

    fn known(
        cx: &mut TestAppContext,
        store: &Entity<AgentsStore>,
        id: &str,
    ) -> Option<(usize, usize)> {
        cx.update(|cx| {
            store
                .read(cx)
                .session_summary("test-agent", id)
                .and_then(digest)
        })
    }

    #[gpui_kit::test]
    fn progress_moves_while_a_background_session_runs(cx: &mut TestAppContext) {
        let (store, requests, generation) =
            store_with(cx, vec![listed("a", &["completed", "pending", "pending"])]);
        assert_eq!(known(cx, &store, "a"), Some((1, 3)));
        plans_changed(
            cx,
            &store,
            generation,
            json!({ "session_id": "a", "plans": todos(&["completed", "completed", "pending"]) }),
        );
        // Long enough for any debounced refresh to have fired.
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        assert_eq!(known(cx, &store, "a"), Some((2, 3)));
        assert!(requests.try_recv().is_err(), "no session/list request");
    }

    #[gpui_kit::test]
    fn plan_emptied(cx: &mut TestAppContext) {
        let (store, requests, generation) = store_with(cx, vec![listed("a", &["pending"])]);
        plans_changed(
            cx,
            &store,
            generation,
            json!({ "session_id": "a", "plans": [] }),
        );
        cx.run_until_parked();
        assert_eq!(known(cx, &store, "a"), None);
        assert!(requests.try_recv().is_err());
    }

    #[gpui_kit::test]
    fn notification_for_an_unknown_session(cx: &mut TestAppContext) {
        let (store, requests, generation) = store_with(cx, vec![listed("a", &["pending"])]);
        plans_changed(
            cx,
            &store,
            generation,
            json!({ "session_id": "ghost", "plans": todos(&["completed"]) }),
        );
        cx.run_until_parked();
        cx.update(|cx| {
            let rt_sessions = store.read(cx).session_summary("test-agent", "ghost");
            assert!(rt_sessions.is_none(), "no session is added");
        });
        assert_eq!(known(cx, &store, "a"), Some((0, 1)), "nothing else changes");
        assert!(requests.try_recv().is_err());
    }

    /// Open `session`; the hub holds it weakly, so the caller keeps the handle.
    fn open(
        cx: &mut TestAppContext,
        store: &Entity<AgentsStore>,
        session: &str,
    ) -> Entity<crate::session::open::OpenSession> {
        let target = SessionRef {
            key: SessionKey::new("test-agent", session),
            cwd: "/w".into(),
            title: session.into(),
        };
        let open = cx.update(|cx| store.update(cx, |s, cx| s.open(target, cx)));
        cx.run_until_parked();
        open
    }

    fn live_plan(
        cx: &mut TestAppContext,
        store: &Entity<AgentsStore>,
        session: &str,
        statuses: &[&str],
    ) {
        let entries: Vec<Value> = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| json!({ "content": format!("live {i}"), "priority": "medium", "status": s }))
            .collect();
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.notify_for_test(
                    "test-agent",
                    "session/update",
                    json!({
                        "sessionId": session,
                        "update": {
                            "sessionUpdate": "plan_update",
                            "plan": { "type": "items", "planId": "querymt-todos", "entries": entries },
                        },
                    }),
                    cx,
                )
            })
        });
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
    }

    fn digest_of(
        cx: &mut TestAppContext,
        store: &Entity<AgentsStore>,
        id: &str,
    ) -> Option<(usize, usize)> {
        cx.update(|cx| {
            let s = store.read(cx);
            let summary = s.session_summary("test-agent", id)?;
            s.plan_digest(summary, cx).map(|d| (d.done, d.total))
        })
    }

    #[gpui_kit::test]
    fn live_plan_wins_over_a_stale_list(cx: &mut TestAppContext) {
        let (store, requests, _) = store_with(
            cx,
            vec![listed(
                "a",
                &["completed", "completed", "pending", "pending"],
            )],
        );
        let _open = open(cx, &store, "a");
        requests.finish_loads(cx);
        live_plan(
            cx,
            &store,
            "a",
            &["completed", "completed", "completed", "pending"],
        );
        assert_eq!(digest_of(cx, &store, "a"), Some((3, 4)));
        // A refresh that still reports 2 of 4 replaces the summary.
        let stale = listed("a", &["completed", "completed", "pending", "pending"]);
        store.update(cx, |s, _| {
            super::super::merge_sessions(s.runtime_mut_for_test("test-agent"), vec![stale])
        });
        assert_eq!(known(cx, &store, "a"), Some((2, 4)));
        assert_eq!(digest_of(cx, &store, "a"), Some((3, 4)));
    }

    #[gpui_kit::test]
    fn open_session_without_a_live_plan_yet(cx: &mut TestAppContext) {
        let (store, _requests, _) = store_with(
            cx,
            vec![listed(
                "a",
                &["completed", "completed", "pending", "pending"],
            )],
        );
        let _open = open(cx, &store, "a");
        assert_eq!(digest_of(cx, &store, "a"), Some((2, 4)));
    }
}
