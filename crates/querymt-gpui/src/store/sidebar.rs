//! What needs the user's attention in the sidebar (desktop
//! `RecentSessionRail` / `SidebarAttentionDot`): sessions waiting on a
//! permission or a question, and agents that failed or lost their connection.

use std::collections::HashSet;

#[cfg(test)]
use gpui_kit::*;

use super::{AgentsStore, ConnState};

impl AgentsStore {
    /// Root sessions (`agent/session`) with a permission or question waiting.
    /// A delegate's request marks the session it belongs to, when known.
    pub fn sessions_waiting(&self) -> HashSet<String> {
        self.requests
            .iter()
            .filter(|r| r.is_pending())
            .filter_map(|r| {
                let mut id = r.session_id.clone()?;
                // Walk up to the root through known parents.
                for _ in 0..16 {
                    match self
                        .session_summary(&r.agent_id, &id)
                        .and_then(|s| s.parent.clone().filter(|_| !s.root))
                    {
                        Some(parent) => id = parent,
                        None => break,
                    }
                }
                Some(format!("{}/{id}", r.agent_id))
            })
            .collect()
    }

    /// Agents in trouble, with why: failed, or connection lost and retrying
    /// (desktop `agentsNeedingAttention`).
    pub fn agents_needing_attention(&self) -> Vec<(String, String)> {
        self.configs
            .iter()
            .filter_map(|c| {
                let rt = self.runtime.get(&c.id)?;
                let why = match &rt.state {
                    ConnState::Failed(err) if err.is_empty() => "failed".to_string(),
                    ConnState::Failed(err) => format!("failed: {err}"),
                    ConnState::Connecting if rt.reconnect_attempt > 0 => {
                        "connection lost, reconnecting".to_string()
                    }
                    _ => return None,
                };
                Some((c.name.clone(), why))
            })
            .collect()
    }
}

#[cfg(test)]
impl AgentsStore {
    /// Deliver request `method` from agent `agent`, as its connection would.
    pub(crate) fn request_for_test(
        &mut self,
        agent: &str,
        id: u64,
        method: &str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let generation = self.runtime.get(agent).map_or(0, |rt| rt.generation);
        let incoming = crate::rpc::Incoming::Request {
            id: serde_json::json!(id),
            method: method.into(),
            params,
        };
        self.on_incoming(agent, generation, incoming, cx);
    }
}
