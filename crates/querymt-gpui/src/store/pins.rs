//! Live pins (desktop `querymt/session/pinsChanged`): another client pinned
//! or unpinned a session. The notification names only the session, so the
//! pinned list is re-read and every local root session is reconciled with
//! it, like the desktop's `ensurePinnedSessions(force)`.

use std::collections::HashSet;
use std::time::Duration;

use gpui_kit::*;
use serde_json::{Value, json};

use super::{
    AgentRuntime, AgentsStore, CATALOG_PAGES, SessionSummary, fetch_session_pages, merge_sessions,
};

/// Notifications arrive in bursts (one per changed session): coalesce.
const PINS_DEBOUNCE: Duration = Duration::from_millis(250);

/// `session/list` `_meta` of every pinned root session, whatever its
/// lifecycle. Pins are local: mesh peers are never asked.
fn pinned_scope() -> Value {
    json!({
        "session_scope": "root",
        "querymt": {
            "sessionFilters": { "resolution": "any", "archive": "any", "pinned": true }
        }
    })
}

/// Pin state of the known sessions from the agent's pinned list: local root
/// sessions are pinned exactly when listed (children and peer sessions are
/// not in that list, so they keep theirs). Pinned sessions the catalog has
/// not loaded yet are added. Sessions with a pin request of ours in flight
/// keep their optimistic state; its response settles them.
fn apply_pins(rt: &mut AgentRuntime, listed: Vec<SessionSummary>) {
    let pinned: HashSet<&str> = listed
        .iter()
        .filter(|s| s.pinned)
        .map(|s| s.id.as_str())
        .collect();
    let pending = &rt.session_pending;
    let mut known: HashSet<String> = HashSet::new();
    for s in rt
        .sessions
        .iter_mut()
        .chain(rt.children.values_mut().flat_map(|c| c.sessions.iter_mut()))
    {
        known.insert(s.id.clone());
        if s.root && s.remote.is_none() && !pending.contains(&s.id) {
            s.pinned = pinned.contains(s.id.as_str());
        }
    }
    let missing: Vec<SessionSummary> = listed
        .into_iter()
        .filter(|s| s.pinned && !known.contains(&s.id))
        .collect();
    if !missing.is_empty() {
        merge_sessions(rt, missing);
    }
}

impl AgentsStore {
    /// `querymt/session/pinsChanged` from agent `id`.
    pub(super) fn on_pins_changed(&mut self, id: &str, params: &Value, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        if !rt.can_pin() || rt.pins_refresh_scheduled {
            return;
        }
        // The echo of our own pin/unpin: its response settles it.
        let session = params.get("session_id").and_then(Value::as_str);
        if session.is_some_and(|s| rt.session_pending.contains(s)) {
            return;
        }
        let Some(rpc) = rt.rpc.clone() else { return };
        rt.pins_refresh_scheduled = true;
        let (generation, id) = (rt.generation, id.to_string());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PINS_DEBOUNCE).await;
            let listed = fetch_session_pages(&rpc, &id, Some(pinned_scope()), CATALOG_PAGES).await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else {
                    return;
                };
                rt.pins_refresh_scheduled = false;
                match listed {
                    Ok(listed) => apply_pins(rt, listed),
                    Err(err) => rt.log(format!("Could not refresh pinned sessions: {err}")),
                }
                cx.notify();
            });
        });
        rt.tasks.push(task);
        rt.tasks.retain(|t| !t.is_ready());
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentRuntime, SessionSummary, apply_pins, pinned_scope};

    fn session(id: &str, pinned: bool, root: bool) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            pinned,
            root,
            updated_at: format!("2026-01-0{}", id.len()),
            ..Default::default()
        }
    }

    #[test]
    fn pins_follow_the_agents_list() {
        let mut rt = AgentRuntime {
            sessions: vec![
                session("a", false, true),
                session("bb", true, true),
                session("pend", false, true),
            ],
            ..Default::default()
        };
        // A fork keeps its own pin: it is not in the root list.
        rt.children.entry("bb".into()).or_default().sessions = vec![session("fork", true, false)];
        rt.session_pending.insert("pend".into());
        apply_pins(
            &mut rt,
            vec![
                session("a", true, true),
                session("ccc", true, true),
                session("pend", true, true),
            ],
        );
        let pinned = |rt: &AgentRuntime, id: &str| {
            rt.sessions
                .iter()
                .chain(rt.children.values().flat_map(|c| c.sessions.iter()))
                .find(|s| s.id == id)
                .map(|s| s.pinned)
        };
        assert_eq!(pinned(&rt, "a"), Some(true), "pinned elsewhere");
        assert_eq!(pinned(&rt, "bb"), Some(false), "unpinned elsewhere");
        assert_eq!(pinned(&rt, "ccc"), Some(true), "not loaded yet: added");
        assert_eq!(
            pinned(&rt, "pend"),
            Some(false),
            "our request in flight wins"
        );
        assert_eq!(pinned(&rt, "fork"), Some(true));
    }

    #[gpui_kit::test]
    fn a_pin_from_another_client_shows_up(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use serde_json::{Value, json};
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.capabilities = Some(json!({ "methods": ["querymt/session/setPinned"] }));
        rt.sessions = vec![session("a", false, true), session("bb", true, true)];
        let generation = rt.generation;
        let store = cx.new(|_| store);
        let notify = |cx: &mut gpui_kit::TestAppContext| {
            store.update(cx, |s, cx| {
                s.on_incoming(
                    "test-agent",
                    generation,
                    crate::rpc::Incoming::Notification {
                        method: "_querymt/session/pinsChanged".into(),
                        params: json!({ "session_id": "a" }),
                    },
                    cx,
                )
            });
        };
        // A burst of notifications makes one request, after the debounce.
        notify(cx);
        notify(cx);
        cx.executor().advance_clock(super::PINS_DEBOUNCE);
        cx.run_until_parked();
        let request: Value = serde_json::from_str(&requests.try_recv().unwrap()).unwrap();
        assert!(requests.try_recv().is_err());
        assert_eq!(request["method"], "session/list");
        assert_eq!(request["params"]["_meta"], pinned_scope());
        rpc.handle(
            &json!({ "id": request["id"], "result": { "sessions": [{
                "sessionId": "a", "cwd": "/w",
                "_meta": { "querymt": { "pin": { "pinned": true } } }
            }] } })
            .to_string(),
        );
        cx.run_until_parked();
        cx.update(|cx| {
            let s = store.read(cx);
            assert_eq!(
                s.session_summary("test-agent", "a").map(|s| s.pinned),
                Some(true)
            );
            assert_eq!(
                s.session_summary("test-agent", "bb").map(|s| s.pinned),
                Some(false)
            );
        });
    }

    #[test]
    fn the_pinned_list_covers_every_lifecycle() {
        let scope = pinned_scope();
        let filters = &scope["querymt"]["sessionFilters"];
        assert_eq!(filters["pinned"], true);
        assert_eq!(
            (filters["resolution"].as_str(), filters["archive"].as_str()),
            (Some("any"), Some("any"))
        );
        assert!(scope.get("remoteNodeIds").is_none());
    }
}
