//! New worktree sessions and worktree handoff from the open session (desktop
//! `createWorktreeSessionFrom` / `handoffWorktreeSession`):
//!
//! - `querymt/worktree/create` starts an isolated session in a new managed
//!   Git worktree; the source session is untouched (lineage only).
//! - `querymt/worktree/handoff` starts a fresh conversation in the same
//!   checkout; the previous owner stays readable but can no longer run.

use std::time::Duration;

use gpui_kit::*;
use serde_json::{Value, json};

use super::{AgentsStore, SessionSummary, StoreEvent, Worktree};
use crate::rpc::RpcError;
use crate::session::open::{SessionKey, SessionRef};

const CREATE: &str = "_querymt/worktree/create";
const HANDOFF: &str = "_querymt/worktree/handoff";
/// `AgentRuntime::pending` key while a worktree action runs.
const PENDING: &str = "worktree";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeAction {
    Create,
    Handoff,
}

/// Options of the worktree dialog (desktop `WorktreeCreateOptions` /
/// `WorktreeHandoffOptions`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeOptions {
    pub name: String,
    /// Create: branch, tag or commit to start from.
    pub base_ref: String,
    /// Create: copy uncommitted work into the worktree.
    pub include_local_changes: bool,
    /// Handoff: seed the new conversation with a summary of this one.
    pub summary: bool,
}

impl WorktreeOptions {
    /// Why the agent would reject these options (desktop
    /// `validateWorktreeCreateOptions`); `None` when they are fine.
    pub fn problem(&self, action: WorktreeAction) -> Option<&'static str> {
        let base_ref = self.base_ref.trim();
        if action == WorktreeAction::Create && self.include_local_changes && !base_ref.is_empty() {
            return Some("Choose either a base ref or include local changes, not both.");
        }
        if self.name.trim().contains(char::is_whitespace) {
            return Some("Name must be a single word without spaces.");
        }
        if action == WorktreeAction::Create && base_ref.contains(char::is_whitespace) {
            return Some("Base ref must be a single word without spaces.");
        }
        None
    }
}

/// `querymt/worktree/create` params from `source_id` in `cwd`.
fn create_params(cwd: &str, source_id: Option<&str>, o: &WorktreeOptions) -> Value {
    let mut params = json!({ "cwd": cwd });
    if let Some(source) = source_id {
        params["source_session_id"] = json!(source);
    }
    if !o.name.trim().is_empty() {
        params["name"] = json!(o.name.trim());
    }
    if o.include_local_changes {
        params["include_local_changes"] = json!(true);
    } else if !o.base_ref.trim().is_empty() {
        params["base_ref"] = json!(o.base_ref.trim());
    }
    params
}

/// `querymt/worktree/handoff` params; the summary only when asked for and
/// supported (clean by default, like the desktop app).
fn handoff_params(session_id: &str, o: &WorktreeOptions, summary_supported: bool) -> Value {
    let mut params = json!({ "session_id": session_id });
    if !o.name.trim().is_empty() {
        params["name"] = json!(o.name.trim());
    }
    if o.summary && summary_supported {
        params["summary"] = json!(true);
    }
    params
}

/// The session a worktree action created.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Created {
    session_id: String,
    session_cwd: Option<String>,
    checkout_path: Option<String>,
    branch: Option<String>,
    baseline_mode: Option<String>,
}

fn parse_created(v: &Value) -> Option<Created> {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    };
    Some(Created {
        session_id: s("session_id").or_else(|| s("sessionId"))?,
        session_cwd: s("session_cwd"),
        checkout_path: s("checkout_path"),
        branch: s("branch"),
        baseline_mode: s("baseline_mode"),
    })
}

/// Actionable text for a failed worktree request (desktop
/// `describeWorktreeError`): known `data.code`s get a fixed explanation,
/// anything else the agent's own message.
fn describe_error(err: &RpcError, action: WorktreeAction) -> String {
    let data = err.raw().and_then(|r| r.get("data"));
    let code = data.and_then(|d| d.get("code")).and_then(Value::as_str);
    let detail = data
        .and_then(|d| d.get("message"))
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty());
    let known = match code {
        Some("owner_active") => Some(
            "Stop the running turn, queued input and any shared delegates before changing this worktree.",
        ),
        Some("owner_transferred") => Some(
            "This worktree was handed off to another session. This conversation is now read-only.",
        ),
        Some("owner_conflict") => Some("This worktree already belongs to another session."),
        Some("workspace_archived") => {
            Some("This worktree is archived. Its session is read-only and can no longer run turns.")
        }
        Some("workspace_missing") => Some("The worktree checkout is missing on disk."),
        Some("workspace_service_unavailable") => Some("Worktrees are not enabled on this agent."),
        Some("isolation_unsupported_layout") => {
            Some("This repository layout cannot be isolated in a worktree.")
        }
        Some("invalid_options") => Some("The worktree options were rejected by the agent."),
        _ => None,
    };
    match (known, detail) {
        (Some(k), Some(d)) if code == Some("invalid_options") => format!("{k} {d}"),
        (Some(k), _) => k.to_string(),
        (None, Some(d)) => d.to_string(),
        (None, None) if !err.to_string().is_empty() => err.to_string(),
        (None, None) => match action {
            WorktreeAction::Create => "Could not create the worktree session.".into(),
            WorktreeAction::Handoff => "Could not hand off the worktree.".into(),
        },
    }
}

impl AgentsStore {
    /// The open session `key`, listed, local and writable on an agent with
    /// managed worktrees.
    fn worktree_source(
        &self,
        key: &SessionKey,
        cx: &App,
    ) -> Option<(&SessionSummary, &super::launch::WorktreeSupport)> {
        self.with_session(key, cx, |v| v.access.is_none())
            .filter(|writable| *writable)?;
        let rt = self
            .runtime
            .get(key.agent.as_str())
            .filter(|rt| rt.is_connected())?;
        let support = rt.worktrees.as_ref()?;
        // Worktree children aren't in the root catalog: they are listed
        // under their parent once it is expanded.
        let summary = rt
            .sessions
            .iter()
            .chain(rt.children.values().flat_map(|c| c.sessions.iter()))
            .find(|s| s.id == key.session.as_str())
            .filter(|s| s.remote.is_none())?;
        Some((summary, support))
    }

    /// "New worktree session" can be offered for the open session.
    pub fn can_create_worktree(&self, key: &SessionKey, cx: &App) -> bool {
        self.worktree_source(key, cx).is_some()
    }

    /// "Hand off worktree" can be offered: the open session owns a
    /// worktree it hasn't handed off yet.
    pub fn can_handoff_worktree(&self, key: &SessionKey, cx: &App) -> bool {
        self.worktree_source(key, cx).is_some_and(|(s, support)| {
            support.handoff
                && s.worktree.as_ref().is_some_and(|w| {
                    w.handed_off_to.is_none()
                        && w.owner_session_id.as_ref().is_none_or(|o| *o == s.id)
                })
        })
    }

    /// The agent can seed a handoff with a summary.
    pub fn handoff_summary_supported(&self, key: &SessionKey, cx: &App) -> bool {
        self.worktree_source(key, cx)
            .is_some_and(|(_, support)| support.handoff_summary)
    }

    /// A worktree action of the open session's agent is in flight.
    pub fn worktree_pending(&self, key: &SessionKey) -> bool {
        self.runtime
            .get(key.agent.as_str())
            .is_some_and(|rt| rt.pending.contains(PENDING))
    }

    /// Run `action` on the open session and open the session it creates.
    /// The task resolves to that session's id, or to the error to show;
    /// `None` when the action can't start now.
    pub fn start_worktree_action(
        &mut self,
        key: &SessionKey,
        action: WorktreeAction,
        options: WorktreeOptions,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<String, String>>> {
        let allowed = match action {
            WorktreeAction::Create => self.can_create_worktree(key, cx),
            WorktreeAction::Handoff => self.can_handoff_worktree(key, cx),
        };
        if !allowed || self.worktree_pending(key) || options.problem(action).is_some() {
            return None;
        }
        let (source, support) = self.worktree_source(key, cx)?;
        let source = source.clone();
        let (method, params) = match action {
            WorktreeAction::Create => {
                // A worktree session's project is where its worktree came from.
                let cwd = source
                    .worktree
                    .as_ref()
                    .and_then(|w| w.source_cwd.clone())
                    .unwrap_or_else(|| source.cwd.clone());
                let lineage = support.source_session.then_some(source.id.as_str());
                (CREATE, create_params(&cwd, lineage, &options))
            }
            WorktreeAction::Handoff => (
                HANDOFF,
                handoff_params(&source.id, &options, support.handoff_summary),
            ),
        };
        let rt = self.runtime.get_mut(&source.agent_id)?;
        let rpc = rt.rpc.clone()?;
        rt.pending.insert(PENDING.into());
        cx.notify();
        let req = rpc.request_detailed(method, params);
        Some(cx.spawn(async move |this, cx| {
            let result = match req.await {
                Ok(v) => parse_created(&v).ok_or_else(|| {
                    "The agent did not return the new worktree session.".to_string()
                }),
                Err(err) => Err(describe_error(&err, action)),
            };
            this.update(cx, |s, cx| {
                s.finish_worktree_action(action, &source, &options, &result, cx)
            })
            .map_err(|_| "The app closed before the worktree session was ready.".to_string())?;
            result.map(|c| c.session_id)
        }))
    }

    fn finish_worktree_action(
        &mut self,
        action: WorktreeAction,
        source: &SessionSummary,
        options: &WorktreeOptions,
        result: &Result<Created, String>,
        cx: &mut Context<Self>,
    ) {
        let agent_id = source.agent_id.as_str();
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        rt.pending.remove(PENDING);
        let Ok(created) = result else {
            cx.notify();
            return;
        };
        let name = options.name.trim();
        let title = match (action, name.is_empty()) {
            (_, false) => name.to_string(),
            (WorktreeAction::Create, true) => format!("Worktree of {}", source.display_title()),
            (WorktreeAction::Handoff, true) => source.display_title(),
        };
        let source_cwd = source
            .worktree
            .as_ref()
            .and_then(|w| w.source_cwd.clone())
            .unwrap_or_else(|| source.cwd.clone());
        let cwd = created
            .session_cwd
            .clone()
            .or_else(|| created.checkout_path.clone())
            .unwrap_or_else(|| source.cwd.clone());
        // Shown at once (desktop `publishWorktreeChild`); the next
        // `session/list` replaces it.
        let child = SessionSummary {
            agent_id: agent_id.to_string(),
            id: created.session_id.clone(),
            cwd: cwd.clone(),
            title: title.clone(),
            phase: "idle".into(),
            open: true,
            lifecycle_known: true,
            parent: Some(source.id.clone()),
            fork_origin: Some("worktree".into()),
            worktree: Some(Worktree {
                branch: created.branch.clone(),
                checkout_path: created.checkout_path.clone(),
                source_cwd: Some(source_cwd),
                baseline_mode: created.baseline_mode.clone().or_else(|| {
                    source
                        .worktree
                        .as_ref()
                        .and_then(|w| w.baseline_mode.clone())
                }),
                lifecycle: Some("ready".into()),
                handed_off_to: None,
                owner_session_id: Some(created.session_id.clone()),
            }),
            ..Default::default()
        };
        if let Some(s) = rt.sessions.iter_mut().find(|s| s.id == source.id) {
            s.has_children = true;
            // The previous owner keeps its history but can no longer run.
            if action == WorktreeAction::Handoff
                && let Some(w) = s.worktree.as_mut()
            {
                w.handed_off_to = Some(created.session_id.clone());
                w.owner_session_id = Some(created.session_id.clone());
            }
        }
        if let Some(list) = rt.children.get_mut(&source.id)
            && !list.sessions.iter().any(|s| s.id == created.session_id)
        {
            list.sessions.insert(0, child.clone());
        }
        if !rt.sessions.iter().any(|s| s.id == created.session_id) {
            rt.sessions.insert(0, child);
        }
        self.schedule_sessions_refresh(agent_id, Duration::from_millis(500), cx);
        cx.emit(StoreEvent::Spawned {
            from: SessionKey::new(agent_id, &source.id),
            child: SessionRef {
                key: SessionKey::new(agent_id, &created.session_id),
                cwd,
                title,
            },
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Created, WorktreeAction, WorktreeOptions, create_params, handoff_params, json,
        parse_created,
    };

    #[test]
    fn options_are_checked_like_the_agent_does() {
        let both = WorktreeOptions {
            base_ref: "main".into(),
            include_local_changes: true,
            ..Default::default()
        };
        assert!(both.problem(WorktreeAction::Create).is_some());
        // Handoff ignores the create-only fields.
        assert_eq!(both.problem(WorktreeAction::Handoff), None);
        let spaced = WorktreeOptions {
            name: "my feature".into(),
            ..Default::default()
        };
        assert!(spaced.problem(WorktreeAction::Handoff).is_some());
        assert_eq!(
            WorktreeOptions::default().problem(WorktreeAction::Create),
            None
        );
    }

    #[test]
    fn requests_carry_only_the_chosen_options() {
        let o = WorktreeOptions {
            name: " feat ".into(),
            base_ref: "v1".into(),
            ..Default::default()
        };
        assert_eq!(
            create_params("/repo", Some("s1"), &o),
            json!({ "cwd": "/repo", "source_session_id": "s1", "name": "feat", "base_ref": "v1" })
        );
        let local = WorktreeOptions {
            include_local_changes: true,
            ..Default::default()
        };
        assert_eq!(
            create_params("/repo", None, &local),
            json!({ "cwd": "/repo", "include_local_changes": true })
        );
        let summary = WorktreeOptions {
            summary: true,
            ..Default::default()
        };
        assert_eq!(
            handoff_params("s1", &summary, true),
            json!({ "session_id": "s1", "summary": true })
        );
        assert_eq!(
            handoff_params("s1", &summary, false),
            json!({ "session_id": "s1" })
        );
    }

    #[test]
    fn created_sessions_are_parsed() {
        assert_eq!(
            parse_created(&json!({
                "session_id": "w1", "session_cwd": "/wt/a", "checkout_path": "/wt/a",
                "branch": "qmt/a", "baseline_mode": "commit", "workspace_id": "ws"
            })),
            Some(Created {
                session_id: "w1".into(),
                session_cwd: Some("/wt/a".into()),
                checkout_path: Some("/wt/a".into()),
                branch: Some("qmt/a".into()),
                baseline_mode: Some("commit".into()),
            })
        );
        assert_eq!(parse_created(&json!({ "branch": "x" })), None);
    }
}
