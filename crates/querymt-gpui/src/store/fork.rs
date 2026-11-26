//! Forking the open session from one of its replies (desktop
//! `forkActiveSessionAt`): `session/fork` with `_meta.querymt.message_id`
//! makes a new, independent session whose history ends at that message. The
//! source session and the workspace stay as they are. Without a message id
//! the agent forks at the session's last stored message (the sidebar's
//! "Fork from last turn").

use std::time::Duration;

use gpui_kit::*;
use serde_json::{Value, json};

use super::{AgentsStore, SessionSummary, StoreEvent};
use crate::session::open::{SessionKey, SessionRef};

/// `AgentRuntime::pending` key while a fork is being created.
const PENDING: &str = "fork";

/// `session/fork` params forking `session_id` at `message_id`, or at its
/// last stored message.
fn fork_params(session_id: &str, cwd: &str, message_id: Option<&str>) -> Value {
    let mut params = json!({
        "sessionId": session_id,
        "cwd": cwd,
        "mcpServers": [],
    });
    if let Some(message_id) = message_id {
        params["_meta"] = json!({ "querymt": { "message_id": message_id } });
    }
    params
}

impl AgentsStore {
    /// The open session's agent advertised `sessionCapabilities.fork`.
    pub fn session_supports_fork(&self, key: &SessionKey, cx: &App) -> bool {
        self.with_session(key, cx, |v| v.access.is_none())
            .is_some_and(|writable| writable)
            && self
                .runtime
                .get(key.agent.as_str())
                .is_some_and(|rt| rt.is_connected() && rt.can_fork_sessions)
    }

    /// A fork of a session of `key`'s agent is being created.
    pub fn fork_pending(&self, key: &SessionKey) -> bool {
        self.fork_pending_for(key.agent.as_str())
    }

    /// Fork the open session at `message_id` and open the fork. The task
    /// resolves to the fork's id, or to the error to show; `None` when no
    /// fork can start now (busy, unsupported, nothing open).
    pub fn fork_session_at(
        &mut self,
        key: &SessionKey,
        message_id: String,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<String, String>>> {
        if !self.session_supports_fork(key, cx) || self.fork_pending(key) {
            return None;
        }
        let (cwd, title) = self.with_session(key, cx, |view| {
            let idle = !(view.loading || view.running() || view.undo_pending);
            idle.then(|| (view.cwd.clone(), view.title.clone()))
        })??;
        let (agent_id, session_id) = (key.agent.to_string(), key.session.to_string());
        self.start_fork(agent_id, session_id, cwd, title, Some(message_id), true, cx)
    }

    /// `s` can be forked from its last turn now: a local, idle session on
    /// a connected agent that forks, with no other fork being created.
    pub fn can_fork_latest(&self, s: &SessionSummary) -> bool {
        s.remote.is_none()
            && !s.running()
            && self.runtime.get(&s.agent_id).is_some_and(|rt| {
                rt.is_connected() && rt.can_fork_sessions && !rt.pending.contains(PENDING)
            })
    }

    /// A fork of a session of `agent_id` is being created.
    pub fn fork_pending_for(&self, agent_id: &str) -> bool {
        self.runtime
            .get(agent_id)
            .is_some_and(|rt| rt.pending.contains(PENDING))
    }

    /// Fork `s` at its last stored message (the end of its last turn). The
    /// caller opens the fork; the task resolves like [`Self::fork_session_at`].
    pub fn fork_session_latest(
        &mut self,
        s: &SessionSummary,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<String, String>>> {
        if !self.can_fork_latest(s) {
            return None;
        }
        let (agent_id, session_id) = (s.agent_id.clone(), s.id.clone());
        let (cwd, title) = (s.cwd.clone(), s.display_title());
        self.start_fork(agent_id, session_id, cwd, title, None, false, cx)
    }

    /// Send `session/fork`. `open`: open the fork when it is made, if its
    /// source is still the open session.
    #[allow(clippy::too_many_arguments)]
    fn start_fork(
        &mut self,
        agent_id: String,
        session_id: String,
        cwd: String,
        title: String,
        message_id: Option<String>,
        open: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<String, String>>> {
        let rt = self.runtime.get_mut(&agent_id)?;
        let rpc = rt.rpc.clone()?;
        rt.pending.insert(PENDING.into());
        cx.notify();
        let req = rpc.request(
            "session/fork",
            fork_params(&session_id, &cwd, message_id.as_deref()),
        );
        Some(cx.spawn(async move |this, cx| {
            let result = req.await.and_then(|resp| {
                resp.get("sessionId")
                    .or_else(|| resp.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| "The agent did not return the fork's session id.".to_string())
            });
            this.update(cx, |s, cx| {
                s.finish_fork(&agent_id, &session_id, &cwd, &title, &result, open, cx);
            })
            .map_err(|_| "The app closed before the fork finished.".to_string())?;
            result.map_err(|err| format!("Could not create the fork: {err}"))
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_fork(
        &mut self,
        agent_id: &str,
        source_id: &str,
        cwd: &str,
        title: &str,
        result: &Result<String, String>,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        rt.pending.remove(PENDING);
        let Ok(fork_id) = result else {
            if let Err(err) = result {
                rt.log_error(format!("Could not fork {source_id}: {err}"));
            }
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
        // A surface that still shows the source may switch to the fork.
        if open {
            cx.emit(StoreEvent::Spawned {
                from: SessionKey::new(agent_id, source_id),
                child: SessionRef {
                    key: SessionKey::new(agent_id, fork_id),
                    cwd: cwd.to_string(),
                    title: fork_title,
                },
            });
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
            fork_params("s1", "/w", Some("m9")),
            json!({
                "sessionId": "s1", "cwd": "/w", "mcpServers": [],
                "_meta": { "querymt": { "message_id": "m9" } },
            })
        );
        // Without one, the agent forks at the last stored message.
        assert_eq!(
            fork_params("s1", "/w", None),
            json!({ "sessionId": "s1", "cwd": "/w", "mcpServers": [] })
        );
    }
}
