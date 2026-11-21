//! Forking the open session from one of its replies (desktop
//! `forkActiveSessionAt`): `session/fork` with `_meta.querymt.message_id`
//! makes a new, independent session whose history ends at that message. The
//! source session and the workspace stay as they are.

use std::time::Duration;

use gpui_kit::*;
use serde_json::{Value, json};

use super::{AgentsStore, SessionSummary};

/// `AgentRuntime::pending` key while a fork is being created.
const PENDING: &str = "fork";

/// `session/fork` params forking `session_id` at `message_id`.
fn fork_params(session_id: &str, cwd: &str, message_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "cwd": cwd,
        "mcpServers": [],
        "_meta": { "querymt": { "message_id": message_id } },
    })
}

impl AgentsStore {
    /// The open session's agent advertised `sessionCapabilities.fork`.
    pub fn session_supports_fork(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|v| self.runtime.get(&v.agent_id))
            .is_some_and(|rt| rt.is_connected() && rt.can_fork_sessions)
    }

    /// A fork of the open session is being created.
    pub fn fork_pending(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|v| self.runtime.get(&v.agent_id))
            .is_some_and(|rt| rt.pending.contains(PENDING))
    }

    /// Fork the open session at `message_id` and open the fork. The task
    /// resolves to the fork's id, or to the error to show; `None` when no
    /// fork can start now (busy, unsupported, nothing open).
    pub fn fork_session_at(
        &mut self,
        message_id: String,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<String, String>>> {
        if !self.session_supports_fork() || self.fork_pending() {
            return None;
        }
        let view = self.session.as_ref()?;
        if view.loading || view.running() || view.undo_pending {
            return None;
        }
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());
        let (cwd, title) = (view.cwd.clone(), view.title.clone());
        let rt = self.runtime.get_mut(&agent_id)?;
        let rpc = rt.rpc.clone()?;
        rt.pending.insert(PENDING.into());
        cx.notify();
        let req = rpc.request("session/fork", fork_params(&session_id, &cwd, &message_id));
        Some(cx.spawn(async move |this, cx| {
            let result = req.await.and_then(|resp| {
                resp.get("sessionId")
                    .or_else(|| resp.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| "The agent did not return the fork's session id.".to_string())
            });
            this.update(cx, |s, cx| {
                s.finish_fork(&agent_id, &session_id, &cwd, &title, &result, cx);
            })
            .map_err(|_| "The app closed before the fork finished.".to_string())?;
            result.map_err(|err| format!("Could not create the fork: {err}"))
        }))
    }

    fn finish_fork(
        &mut self,
        agent_id: &str,
        source_id: &str,
        cwd: &str,
        title: &str,
        result: &Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        rt.pending.remove(PENDING);
        let Ok(fork_id) = result else {
            cx.notify();
            return;
        };
        // Show the fork at once; the next `session/list` replaces these.
        let source = rt.sessions.iter_mut().find(|s| s.id == source_id);
        let base = source.as_ref().map(|s| (**s).clone());
        if let Some(source) = source {
            source.fork_count += 1;
            source.has_children = true;
        }
        let fork_title = format!("Fork of {title}");
        // Like a launch placeholder: newest first until the list has it.
        let fork = SessionSummary {
            agent_id: agent_id.to_string(),
            id: fork_id.clone(),
            cwd: cwd.to_string(),
            title: fork_title.clone(),
            phase: "idle".into(),
            open: true,
            lifecycle_known: true,
            parent: Some(source_id.to_string()),
            fork_origin: Some("user".into()),
            worktree: base.as_ref().and_then(|b| b.worktree.clone()),
            remote: base.and_then(|b| b.remote),
            ..Default::default()
        };
        // An expanded source lists its children: the fork goes first there.
        if let Some(list) = rt.children.get_mut(source_id)
            && !list.sessions.iter().any(|s| s.id == *fork_id)
        {
            list.sessions.insert(0, fork.clone());
        }
        if !rt.sessions.iter().any(|s| s.id == *fork_id) {
            rt.sessions.insert(0, fork);
        }
        self.schedule_sessions_refresh(agent_id, Duration::from_millis(500), cx);
        // Open the fork only while its source is still the open session.
        let still_open = self
            .session
            .as_ref()
            .is_some_and(|v| v.agent_id == agent_id && v.session_id == source_id);
        if still_open {
            self.open_session(agent_id, fork_id, cwd, &fork_title, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{fork_params, json};

    #[test]
    fn fork_params_name_the_message() {
        assert_eq!(
            fork_params("s1", "/w", "m9"),
            json!({
                "sessionId": "s1", "cwd": "/w", "mcpServers": [],
                "_meta": { "querymt": { "message_id": "m9" } },
            })
        );
    }
}
