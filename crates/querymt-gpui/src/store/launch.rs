//! Where a new session runs (desktop `composerTargetId` and
//! `composerWorkspaceMode`): this machine or a mesh peer, and the local
//! checkout or a new git worktree. Also the profiles of a mesh peer, which a
//! session started there can use.

use std::collections::HashMap;

use gpui_kit::*;
use serde_json::{Value, json};

use super::{AgentsStore, DEFAULT_PROFILE, Profile};

/// Profile id meaning "the owner's active profile" on a mesh peer.
pub const OWNER_DEFAULT_PROFILE: &str = "querymt:owner-default";

/// Where the session's files live (desktop `ComposerWorkspaceMode`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorktreeMode {
    /// The folder itself.
    #[default]
    Local,
    /// A new git worktree from the latest commit.
    Worktree,
    /// A new git worktree that starts with the folder's uncommitted changes.
    WorktreeWithChanges,
}

impl WorktreeMode {
    pub const ALL: [(Self, &'static str, &'static str); 3] = [
        (Self::Local, "Checkout", "Runs in the folder itself."),
        (
            Self::Worktree,
            "New worktree",
            "Runs in a new git worktree from the latest commit, so your checkout stays untouched.",
        ),
        (
            Self::WorktreeWithChanges,
            "With changes",
            "Runs in a new git worktree that starts with your uncommitted changes.",
        ),
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "Local checkout",
            Self::Worktree => "New worktree",
            Self::WorktreeWithChanges => "New worktree with local changes",
        }
    }

    /// `_meta.querymt.sessionWorkspace` of `session/new`.
    pub fn request(self) -> Option<Value> {
        match self {
            Self::Local => None,
            Self::Worktree => Some(json!({ "execution": "worktree" })),
            Self::WorktreeWithChanges => {
                Some(json!({ "execution": "worktree", "includeLocalChanges": true }))
            }
        }
    }
}

/// Managed worktrees advertised in `initialize`
/// (`agentCapabilities._meta.querymt.workspace`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorktreeSupport {
    /// `querymt/worktree/handoff` exists.
    pub handoff: bool,
    /// Handoff accepts `summary: true`.
    pub handoff_summary: bool,
    /// `querymt/worktree/create` accepts `source_session_id` (lineage).
    pub source_session: bool,
}

pub fn worktree_support(init: &Value) -> Option<WorktreeSupport> {
    let workspace = init.pointer("/agentCapabilities/_meta/querymt/workspace")?;
    let flag = |k: &str| workspace.get(k).and_then(Value::as_bool) == Some(true);
    flag("supported").then(|| WorktreeSupport {
        handoff: flag("handoff"),
        handoff_summary: flag("handoff") && flag("handoffSummary"),
        source_session: flag("sourceSession"),
    })
}

/// Profiles of one mesh peer, as listed through the agent.
#[derive(Clone, Debug, Default)]
pub struct PeerProfiles {
    pub loading: bool,
    pub profiles: Vec<Profile>,
    pub error: Option<String>,
}

/// A launch target: this machine (`None`) or a mesh peer.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetChoice {
    pub node_id: Option<String>,
    pub label: String,
    pub detail: String,
    /// A peer chosen earlier that the mesh no longer lists.
    pub unavailable: bool,
}

/// What a mesh host is doing, for the Target select. The agent reports
/// `transport` as `unknown` for every peer today, so it is shown only when it
/// says something.
fn peer_detail(transport: &str, active: u64) -> String {
    let transport = Some(transport.trim()).filter(|t| !t.is_empty() && *t != "unknown");
    let sessions = match active {
        0 => "No active sessions".to_string(),
        1 => "1 active session".to_string(),
        n => format!("{n} active sessions"),
    };
    match transport {
        Some(t) => format!("{sessions} · {t}"),
        None => sessions,
    }
}

/// `session/new` params: workspace, profile and worktree placement.
pub fn new_session_params(cwd: &str, profile: &str, worktree: WorktreeMode) -> Value {
    let mut params = json!({ "cwd": cwd, "mcpServers": [] });
    let mut querymt = serde_json::Map::new();
    if profile != DEFAULT_PROFILE {
        querymt.insert("profile_id".into(), json!(profile));
    }
    if let Some(workspace) = worktree.request() {
        querymt.insert("sessionWorkspace".into(), workspace);
    }
    if !querymt.is_empty() {
        params["_meta"] = json!({ "querymt": querymt });
    }
    params
}

/// `_querymt/remote/createSession` params; the owner's active profile is
/// asked for by leaving `profile_id` out.
pub fn remote_session_params(node: &str, cwd: &str, profile: Option<&str>) -> Value {
    let mut params = json!({ "node_id": node, "cwd": cwd, "attach": true });
    if let Some(profile) = profile.filter(|p| *p != OWNER_DEFAULT_PROFILE) {
        params["profile_id"] = json!(profile);
    }
    params
}

/// List `node`'s profiles through the agent (`_querymt/profiles` with
/// `node_id`); the owner must confirm it answered for that peer.
pub fn request_peer_profiles(
    rpc: &crate::rpc::RpcClient,
    node: &str,
) -> impl std::future::Future<Output = Result<Vec<Profile>, String>> + 'static {
    let node = node.to_owned();
    let request = rpc.request("_querymt/profiles", json!({ "node_id": node }));
    async move {
        request.await.and_then(|v| {
            if v.get("node_id").and_then(Value::as_str) == Some(node.as_str()) {
                Ok(super::parse_profiles(&v))
            } else {
                Err("The agent did not confirm the peer's profiles. Update both agents.".into())
            }
        })
    }
}

/// Where the created session runs, when the agent moved it (a worktree).
pub fn session_cwd(created: &Value) -> Option<String> {
    created
        .pointer("/_meta/querymt/sessionWorkspace/sessionCwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
}

impl AgentsStore {
    /// Worktrees are offered: a local target on an agent that supports them.
    pub fn can_use_worktree(&self, agent_id: &str) -> bool {
        self.launch.target.is_none()
            && self
                .runtime(agent_id)
                .is_some_and(|rt| rt.worktrees.is_some())
    }

    /// Worktree placement for the next session, valid for `agent_id`.
    pub fn launch_worktree(&self, agent_id: &str) -> WorktreeMode {
        if self.can_use_worktree(agent_id) {
            self.launch.worktree
        } else {
            WorktreeMode::Local
        }
    }

    pub fn set_launch_worktree(&mut self, mode: WorktreeMode, cx: &mut Context<Self>) {
        self.launch.worktree = mode;
        cx.notify();
    }

    /// This machine, then the agent's mesh peers; a chosen peer that left
    /// the mesh stays listed as unavailable.
    pub fn launch_targets(&self, agent_id: &str) -> Vec<TargetChoice> {
        let mut out = vec![TargetChoice {
            node_id: None,
            label: "Local".into(),
            detail: "On the agent's own machine.".into(),
            unavailable: false,
        }];
        let Some(rt) = self.runtime(agent_id) else {
            return out;
        };
        let can_create = rt.supports_method("querymt/remote/createSession");
        if can_create {
            out.extend(rt.mesh_nodes.iter().map(|node| TargetChoice {
                node_id: Some(node.id.clone()),
                label: if node.label.is_empty() {
                    node.id.clone()
                } else {
                    node.label.clone()
                },
                detail: peer_detail(&node.transport, node.active_sessions),
                unavailable: false,
            }));
        }
        if let Some(target) = &self.launch.target
            && !out.iter().any(|t| t.node_id.as_ref() == Some(target))
        {
            let label = rt
                .sessions
                .iter()
                .filter_map(|s| s.remote.as_ref())
                .find(|r| &r.node_id == target)
                .map_or_else(|| target.clone(), |r| r.name());
            out.push(TargetChoice {
                node_id: Some(target.clone()),
                label,
                detail: "Not reachable on the mesh right now.".into(),
                unavailable: true,
            });
        }
        out
    }

    /// Run the next session on `node` (`None`: this machine). A peer has its
    /// own profiles and no worktrees.
    pub fn set_launch_target(
        &mut self,
        agent_id: &str,
        node: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.launch.target != node {
            self.launch.remote_profile = None;
        }
        self.launch.target = node.clone();
        if let Some(node) = node {
            self.load_peer_profiles(agent_id, &node, cx);
        }
        cx.notify();
    }

    pub fn set_launch_remote_profile(&mut self, profile: Option<String>, cx: &mut Context<Self>) {
        self.launch.remote_profile = profile.filter(|p| p != OWNER_DEFAULT_PROFILE);
        cx.notify();
    }

    pub fn peer_profiles(&self, agent_id: &str, node: &str) -> Option<&PeerProfiles> {
        self.runtime(agent_id)?.peer_profiles.get(node)
    }

    /// List `node`'s profiles through the agent (`_querymt/profiles` with
    /// `node_id`).
    pub fn load_peer_profiles(&mut self, agent_id: &str, node: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else { return };
        if !rt.supports_method("querymt/profiles") {
            return;
        }
        let entry = rt.peer_profiles.entry(node.to_owned()).or_default();
        entry.loading = true;
        entry.error = None;
        let (agent_id, node) = (agent_id.to_owned(), node.to_owned());
        let request = request_peer_profiles(&rpc, &node);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |s, cx| {
                s.store_peer_profiles(&agent_id, &node, &result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Record a fresh profile list (or its error) for `node`.
    pub(super) fn store_peer_profiles(
        &mut self,
        agent_id: &str,
        node: &str,
        result: &Result<Vec<Profile>, String>,
    ) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        let entry = rt.peer_profiles.entry(node.to_owned()).or_default();
        entry.loading = false;
        match result {
            Ok(profiles) => {
                entry.profiles = profiles.clone();
                entry.error = None;
            }
            Err(error) => {
                entry.profiles.clear();
                entry.error = Some(error.clone());
            }
        }
    }

    /// What the placeholder says while the next session is being created
    /// (desktop `showLaunchProgress`).
    pub fn launch_progress_label(&self) -> String {
        match &self.launch.creating_on {
            Some(peer) => format!("Creating session on {peer}…"),
            None => "Creating session…".into(),
        }
    }
}

/// Peer profile lists, keyed by node id.
pub type PeerProfileMap = HashMap<String, PeerProfiles>;

#[cfg(test)]
impl AgentsStore {
    pub(crate) fn runtime_mut_for_test(&mut self, agent: &str) -> &mut super::AgentRuntime {
        self.runtime.entry(agent.to_owned()).or_default()
    }

    /// Connect `agent` to a test channel: requests arrive on the receiver,
    /// replies go through the returned client's `handle`.
    pub(crate) fn rpc_for_test(
        &mut self,
        agent: &str,
    ) -> (
        std::rc::Rc<crate::rpc::RpcClient>,
        async_channel::Receiver<String>,
    ) {
        let (sender, requests) = async_channel::unbounded();
        let rpc = crate::rpc::RpcClient::new(sender);
        self.runtime_mut_for_test(agent).rpc = Some(rpc.clone());
        (rpc, requests)
    }
}

/// A connected agent with one mesh peer (`thor`), worktrees and profiles.
#[cfg(test)]
pub(crate) fn peer_fixture() -> AgentsStore {
    let mut store = super::automations::schedule_ui_fixture();
    let rt = store.runtime.get_mut("test-agent").expect("fixture agent");
    rt.capabilities = Some(json!({
        "methods": ["querymt/remote/createSession", "querymt/profiles"]
    }));
    rt.worktrees = Some(WorktreeSupport::default());
    rt.profiles = vec![Profile {
        id: "dev".into(),
        name: "Developer".into(),
        description: String::new(),
    }];
    rt.mesh_nodes = vec![super::MeshNode {
        id: "thor-id".into(),
        label: "thor".into(),
        active_sessions: 2,
        capabilities: Vec::new(),
        transport: "iroh".into(),
    }];
    rt.peer_profiles.insert(
        "thor-id".into(),
        PeerProfiles {
            profiles: vec![Profile {
                id: "coder".into(),
                name: "Coder".into(),
                description: String::new(),
            }],
            ..Default::default()
        },
    );
    store
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's prelude brings its own `test` attribute.
    use super::{
        DEFAULT_PROFILE, OWNER_DEFAULT_PROFILE, Value, WorktreeMode, WorktreeSupport, json,
        new_session_params, remote_session_params, session_cwd, worktree_support,
    };

    #[test]
    fn new_session_meta_carries_profile_and_worktree() {
        assert_eq!(
            new_session_params("/w", DEFAULT_PROFILE, WorktreeMode::Local),
            json!({ "cwd": "/w", "mcpServers": [] })
        );
        let p = new_session_params("/w", "review", WorktreeMode::WorktreeWithChanges);
        assert_eq!(
            p.pointer("/_meta/querymt/profile_id"),
            Some(&json!("review"))
        );
        assert_eq!(
            p.pointer("/_meta/querymt/sessionWorkspace"),
            Some(&json!({ "execution": "worktree", "includeLocalChanges": true }))
        );
        let w = new_session_params("/w", DEFAULT_PROFILE, WorktreeMode::Worktree);
        assert_eq!(w.pointer("/_meta/querymt/profile_id"), None);
        assert_eq!(
            w.pointer("/_meta/querymt/sessionWorkspace/execution"),
            Some(&json!("worktree"))
        );
    }

    /// The request each launch target sends first.
    fn first_launch_request(
        cx: &mut gpui_kit::TestAppContext,
        setup: impl FnOnce(&mut super::AgentsStore),
    ) -> Value {
        use gpui_kit::AppContext as _;
        let (sender, requests) = async_channel::unbounded();
        let store = cx.new(|_| {
            let mut store = crate::store::automations::schedule_ui_fixture();
            let rt = store.runtime.get_mut("test-agent").unwrap();
            rt.rpc = Some(crate::rpc::RpcClient::new(sender));
            rt.capabilities = Some(json!({ "methods": ["querymt/remote/createSession"] }));
            rt.worktrees = Some(WorktreeSupport::default());
            rt.mesh_nodes = vec![crate::store::MeshNode {
                id: "thor-id".into(),
                label: "thor".into(),
                active_sessions: 0,
                capabilities: Vec::new(),
                transport: "iroh".into(),
            }];
            setup(&mut store);
            store
        });
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.launch("test-agent", "/work".into(), "hi".into(), Vec::new(), cx)
            })
        });
        cx.run_until_parked();
        serde_json::from_str(&requests.try_recv().unwrap()).unwrap()
    }

    #[gpui_kit::test]
    fn launch_targets_a_peer_or_a_worktree(cx: &mut gpui_kit::TestAppContext) {
        let peer = first_launch_request(cx, |s| {
            assert_eq!(s.launch_targets("test-agent").len(), 2);
            s.launch.target = Some("thor-id".into());
            s.launch.remote_profile = Some("coder".into());
            s.launch.worktree = WorktreeMode::Worktree;
        });
        assert_eq!(peer["method"], "_querymt/remote/createSession");
        assert_eq!(peer["params"]["node_id"], "thor-id");
        assert_eq!(peer["params"]["profile_id"], "coder");
        // A peer never gets worktree placement.
        assert!(peer["params"].get("_meta").is_none());

        let local = first_launch_request(cx, |s| {
            s.launch.worktree = WorktreeMode::WorktreeWithChanges
        });
        assert_eq!(local["method"], "session/new");
        assert_eq!(
            local.pointer("/params/_meta/querymt/sessionWorkspace/includeLocalChanges"),
            Some(&json!(true))
        );

        // Without worktree support the choice is not sent.
        let plain = first_launch_request(cx, |s| {
            s.launch.worktree = WorktreeMode::Worktree;
            s.runtime.get_mut("test-agent").unwrap().worktrees = None;
        });
        assert!(plain["params"].get("_meta").is_none());
    }

    #[gpui_kit::test]
    fn placeholder_names_the_peer_while_creating(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| super::peer_fixture());
        let _requests = cx.update(|cx| {
            store.update(cx, |s, cx| {
                let (_, requests) = s.rpc_for_test("test-agent");
                s.launch.target = Some("thor-id".into());
                s.launch("test-agent", "/w".into(), "hi".into(), Vec::new(), cx);
                assert_eq!(s.launch_progress_label(), "Creating session on thor…");
                s.launch.creating_on = None;
                assert_eq!(s.launch_progress_label(), "Creating session…");
                requests
            })
        });
    }

    #[test]
    fn peer_detail_hides_unknown_transport() {
        assert_eq!(super::peer_detail("unknown", 16), "16 active sessions");
        assert_eq!(super::peer_detail("", 0), "No active sessions");
        assert_eq!(super::peer_detail("iroh", 1), "1 active session · iroh");
    }

    #[test]
    fn remote_session_omits_owner_default_profile() {
        let p = remote_session_params("n1", "/srv", None);
        assert_eq!(p, json!({ "node_id": "n1", "cwd": "/srv", "attach": true }));
        let p = remote_session_params("n1", "/srv", Some(OWNER_DEFAULT_PROFILE));
        assert!(p.get("profile_id").is_none());
        let p = remote_session_params("n1", "/srv", Some("coder"));
        assert_eq!(p["profile_id"], "coder");
    }

    #[test]
    fn reads_worktree_support_and_session_cwd() {
        let parse = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        let init = parse(
            r#"{"agentCapabilities":{"_meta":{"querymt":{"workspace":{"supported":true,"handoff":true}}}}}"#,
        );
        assert_eq!(
            worktree_support(&init),
            Some(WorktreeSupport {
                handoff: true,
                ..Default::default()
            })
        );
        assert_eq!(worktree_support(&json!({})), None);
        let created = parse(r#"{"_meta":{"querymt":{"sessionWorkspace":{"sessionCwd":"/wt/a"}}}}"#);
        assert_eq!(session_cwd(&created).as_deref(), Some("/wt/a"));
    }
}
