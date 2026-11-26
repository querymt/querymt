//! MCP servers a session had to leave out (desktop `skippedMcpServers`):
//! their OAuth login is missing or could not be checked, so their tools are
//! not in the session. Session responses (`_meta.querymt.skippedMcpServers`
//! of `session/new` / `session/load`) are authoritative and replace the list;
//! `querymt/session/mcpAuthRequired` notifications add to it.

use gpui_kit::*;
use serde_json::Value;

use super::{AgentRuntime, AgentsStore};
use crate::session::open::SessionKey;

/// One skipped server (querymt `SkippedMcpServer`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedMcp {
    pub server: String,
    /// Auth id to sign in with (`mcp:<server>`).
    pub provider: String,
    /// Why authorization could not be used, when it isn't just "no login yet".
    pub error: Option<String>,
}

impl SkippedMcp {
    fn parse(v: &Value) -> Option<Self> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let server = s("server_name")?;
        Some(Self {
            provider: s("provider_id").unwrap_or_else(|| format!("mcp:{server}")),
            error: s("error"),
            server,
        })
    }

    /// Notice text (desktop `describeSkippedMcpServer`).
    pub fn describe(&self) -> String {
        match &self.error {
            Some(err) => format!("MCP server \"{}\" is unavailable: {err}", self.server),
            None => format!(
                "MCP server \"{}\" needs sign-in. Its tools are not available in this session.",
                self.server
            ),
        }
    }
}

/// `_meta.querymt.skippedMcpServers` of a session response; none listed
/// means none skipped.
pub(crate) fn from_response(v: &Value) -> Vec<SkippedMcp> {
    v.pointer("/_meta/querymt/skippedMcpServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(SkippedMcp::parse)
        .collect()
}

/// Record `servers` for `session`: replace (a response) or merge by server
/// name, later entries winning (a notification).
pub(super) fn record(rt: &mut AgentRuntime, session: &str, servers: Vec<SkippedMcp>, merge: bool) {
    let mut list = if merge {
        rt.skipped_mcp.remove(session).unwrap_or_default()
    } else {
        Vec::new()
    };
    for server in servers {
        match list.iter_mut().find(|s| s.server == server.server) {
            Some(slot) => *slot = server,
            None => list.push(server),
        }
    }
    if list.is_empty() {
        rt.skipped_mcp.remove(session);
    } else {
        rt.skipped_mcp.insert(session.to_string(), list);
    }
}

impl AgentsStore {
    /// Skipped MCP servers of the open session, for its sign-in notice.
    pub fn skipped_mcp(&self, key: &SessionKey) -> Vec<SkippedMcp> {
        self.runtime
            .get(key.agent.as_str())
            .and_then(|rt| rt.skipped_mcp.get(key.session.as_str()).cloned())
            .unwrap_or_default()
    }

    /// The agent reports a stored login for this `mcp:<server>` id.
    pub fn mcp_signed_in(&self, agent_id: &str, provider: &str) -> bool {
        self.runtime.get(agent_id).is_some_and(|rt| {
            rt.mcp_servers
                .iter()
                .any(|s| s.provider == provider && s.oauth_status.as_deref() == Some("connected"))
        })
    }

    /// Hide one server from the open session's notice.
    pub fn dismiss_skipped_mcp(&mut self, key: &SessionKey, server: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(key.agent.as_str())
            && let Some(list) = rt.skipped_mcp.get_mut(key.session.as_str())
        {
            list.retain(|s| s.server != server);
            if list.is_empty() {
                rt.skipped_mcp.remove(key.session.as_str());
            }
            cx.notify();
        }
    }

    /// Deliver notification `method` from agent `agent`, as its connection would.
    #[cfg(test)]
    pub(crate) fn notify_for_test(
        &mut self,
        agent: &str,
        method: &str,
        params: Value,
        cx: &mut Context<Self>,
    ) {
        let generation = self.runtime.get(agent).map_or(0, |rt| rt.generation);
        let incoming = crate::rpc::Incoming::Notification {
            method: method.into(),
            params,
        };
        self.on_incoming(agent, generation, incoming, cx);
    }

    /// `querymt/session/mcpAuthRequired` from agent `id`.
    pub(super) fn on_mcp_auth_required(
        &mut self,
        id: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) {
        let session = params.get("session_id").and_then(Value::as_str);
        let (Some(session), Some(server)) = (session, SkippedMcp::parse(params)) else {
            return;
        };
        if let Some(rt) = self.runtime.get_mut(id) {
            record(rt, session, vec![server], true);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentRuntime, SkippedMcp, from_response, record};
    use serde_json::json;

    fn skipped(server: &str, error: Option<&str>) -> SkippedMcp {
        SkippedMcp {
            server: server.into(),
            provider: format!("mcp:{server}"),
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn responses_replace_and_notifications_merge() {
        let response = json!({ "_meta": { "querymt": { "skippedMcpServers": [
            { "server_name": "linear", "provider_id": "mcp:linear" },
            { "server_name": "  " }
        ] } } });
        let mut rt = AgentRuntime::default();
        record(&mut rt, "s", from_response(&response), false);
        assert_eq!(rt.skipped_mcp["s"], [skipped("linear", None)]);
        // A notification about the same server updates it; a new one joins.
        record(&mut rt, "s", vec![skipped("linear", Some("offline"))], true);
        record(&mut rt, "s", vec![skipped("notion", None)], true);
        assert_eq!(
            rt.skipped_mcp["s"],
            [skipped("linear", Some("offline")), skipped("notion", None)]
        );
        // The next response is authoritative.
        record(&mut rt, "s", from_response(&json!({})), false);
        assert!(!rt.skipped_mcp.contains_key("s"));
    }

    #[test]
    fn the_notice_says_what_happened() {
        assert!(skipped("linear", None).describe().contains("needs sign-in"));
        assert!(
            skipped("linear", Some("timeout"))
                .describe()
                .ends_with("unavailable: timeout")
        );
    }
}
