//! Live mesh membership (desktop `refreshMeshAvailability` on
//! `querymt/mesh/joined` and `querymt/mesh/peerExpired`): this node joined a
//! mesh, or a peer dropped out. Mesh status and peers are re-read, and
//! sessions too when peers' sessions are listed, so the Mesh page, the Start
//! target list and remote sessions follow without a manual refresh.

use std::time::Duration;

use gpui_kit::*;

use super::AgentsStore;

/// Expiries and joins come in bursts as the mesh settles: coalesce.
const MESH_DEBOUNCE: Duration = Duration::from_millis(300);

/// `_querymt` prefix stripped: the notifications that change membership.
pub(super) fn is_membership_change(method: &str) -> bool {
    matches!(
        method.trim_start_matches('_'),
        "querymt/mesh/joined" | "querymt/mesh/peerExpired"
    )
}

impl AgentsStore {
    /// Membership of agent `id`'s mesh changed.
    pub(super) fn on_mesh_membership_changed(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        if !rt.is_connected() || rt.mesh_refresh_scheduled {
            return;
        }
        rt.mesh_refresh_scheduled = true;
        let (generation, id) = (rt.generation, id.to_string());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MESH_DEBOUNCE).await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else {
                    return;
                };
                rt.mesh_refresh_scheduled = false;
                s.refresh_extras(&id, cx);
                // Peers' sessions are listed: their availability changed.
                if !s.prefs.remote_node_ids().is_empty() {
                    s.schedule_sessions_refresh(&id, Duration::ZERO, cx);
                }
            });
        });
        rt.tasks.push(task);
        rt.tasks.retain(|t| !t.is_ready());
    }
}

#[cfg(test)]
mod tests {
    use super::is_membership_change;

    #[test]
    fn joins_and_expiries_change_membership() {
        assert!(is_membership_change("_querymt/mesh/peerExpired"));
        assert!(is_membership_change("querymt/mesh/joined"));
        // nodesChanged keeps its full refresh (it also changes models).
        assert!(!is_membership_change("_querymt/mesh/nodesChanged"));
    }

    #[gpui_kit::test]
    fn an_expired_peer_re_reads_the_mesh(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use serde_json::{Value, json};
        let mut store = crate::store::launch::peer_fixture();
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.capabilities = Some(json!({ "features": { "mesh": true } }));
        let generation = rt.generation;
        let store = cx.new(|_| store);
        for _ in 0..3 {
            store.update(cx, |s, cx| {
                s.on_incoming(
                    "test-agent",
                    generation,
                    crate::rpc::Incoming::Notification {
                        method: "_querymt/mesh/peerExpired".into(),
                        params: json!({ "peer_id": "thor-id" }),
                    },
                    cx,
                )
            });
        }
        // Nothing before the debounce, then one mesh status read.
        cx.run_until_parked();
        assert!(requests.try_recv().is_err());
        cx.executor().advance_clock(super::MESH_DEBOUNCE);
        cx.run_until_parked();
        let first: Value = serde_json::from_str(&requests.try_recv().unwrap()).unwrap();
        assert_eq!(first["method"], "_querymt/mesh/status");
        assert!(requests.try_recv().is_err(), "one burst, one refresh");
    }
}
