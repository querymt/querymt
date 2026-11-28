//! Where a new session runs (desktop `composerTargetId` and
//! `composerWorkspaceMode`): this machine or a mesh peer, and the local
//! checkout or a new git worktree. Also the profiles of a mesh peer, which a
//! session started there can use.

use std::collections::HashMap;

use gpui_kit::*;
use serde_json::{Value, json};

use std::fmt;

use super::{AgentsStore, Attachment, DEFAULT_PROFILE, Profile};

/// Profile id meaning "the owner's active profile" on a mesh peer.
pub const OWNER_DEFAULT_PROFILE: &str = "querymt:owner-default";

/// Where a new session runs: the start page's choice, or [`Placement::local`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// Mesh peer the session runs on; `None`: the agent's machine.
    pub target: Option<String>,
    /// Profile on the target peer; `None`: the peer's active profile.
    pub remote_profile: Option<String>,
    /// Worktree placement of a local session.
    pub worktree: WorktreeMode,
}

impl Placement {
    /// The agent's own machine, in the folder itself.
    #[allow(dead_code)] // the quick panel launches with it (group 6)
    pub fn local() -> Self {
        Self::default()
    }
}

/// What a surface asks the store to start (design D5).
pub struct LaunchRequest {
    pub agent_id: String,
    pub cwd: String,
    pub prompt: String,
    pub attachments: Vec<Attachment>,
    pub placement: Placement,
    /// Selection key ([`Model::key`]) of the model to start on; `None` uses
    /// the launch default of the agent.
    pub model: Option<String>,
    /// Reasoning effort to start with; `None` uses the launch default.
    pub reasoning: Option<String>,
    /// Agent mode to start in; `None` uses the launch default. The default
    /// itself is not changed by a launch.
    pub mode: Option<String>,
}

/// Why a launch did not produce a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchError {
    /// The agent has no live connection.
    NotConnected,
    /// The agent refused or failed to create the session.
    Create(String),
    /// The profile chosen on the peer is gone from it.
    PeerProfileGone,
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("The selected agent is not connected."),
            Self::Create(reason) => f.write_str(reason),
            Self::PeerProfileGone => f.write_str(MISSING_PEER_PROFILE),
        }
    }
}

/// A launch as its caller sees it: what to show while it runs and after.
#[derive(Clone, Debug, Default)]
pub struct LaunchProgress {
    pub busy: bool,
    pub error: Option<String>,
    /// Id of the session the last launch created.
    pub last_started: Option<String>,
    /// Peer the session being created runs on, for the placeholder.
    pub creating_on: Option<String>,
}

impl LaunchProgress {
    /// What the placeholder says while the next session is being created
    /// (desktop `showLaunchProgress`).
    pub fn label(&self) -> String {
        match &self.creating_on {
            Some(peer) => format!("Creating session on {peer}…"),
            None => "Creating session…".into(),
        }
    }
}

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

/// Shown when the peer profile chosen for a launch is gone from the peer.
pub const MISSING_PEER_PROFILE: &str =
    "Selected profile is not available on the session owner. Choose an owner profile.";

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

    /// The start page's current choice of where the next session runs.
    pub fn placement(&self) -> Placement {
        Placement {
            target: self.launch.target.clone(),
            remote_profile: self.launch.remote_profile.clone(),
            worktree: self.launch.worktree,
        }
    }

    /// Worktree placement of `placement` for `agent_id`: only a local session
    /// on an agent that supports worktrees gets one.
    pub(super) fn worktree_for(&self, agent_id: &str, placement: &Placement) -> WorktreeMode {
        let supported = self
            .runtime(agent_id)
            .is_some_and(|rt| rt.worktrees.is_some());
        if placement.target.is_none() && supported {
            placement.worktree
        } else {
            WorktreeMode::Local
        }
    }

    /// Peer a launch with `placement` creates its session on, for the
    /// placeholder; `None` when it runs on the agent's machine.
    pub fn creating_on(&self, agent_id: &str, placement: &Placement) -> Option<String> {
        let node = placement.target.as_ref()?;
        let label = self
            .launch_targets(agent_id)
            .into_iter()
            .find(|t| t.node_id.as_ref() == Some(node))
            .map(|t| t.label);
        Some(label.unwrap_or_else(|| node.clone()))
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
}

/// Peer profile lists, keyed by node id.
pub type PeerProfileMap = HashMap<String, PeerProfiles>;

#[cfg(test)]
impl AgentsStore {
    pub(crate) fn runtime_mut_for_test(&mut self, agent: &str) -> &mut super::AgentRuntime {
        self.runtime.entry(agent.to_owned()).or_default()
    }

    /// `view` as an open session that was not loaded.
    pub(crate) fn adopt_for_test(
        &mut self,
        view: crate::session::SessionView,
        cx: &mut gpui_kit::Context<Self>,
    ) -> gpui_kit::Entity<crate::session::open::OpenSession> {
        self.adopt(view, cx)
    }

    /// `view` as an open session that the next window shows in its main pane.
    pub(crate) fn install_main_for_test(
        &mut self,
        view: crate::session::SessionView,
        cx: &mut gpui_kit::Context<Self>,
    ) -> gpui_kit::Entity<crate::session::open::OpenSession> {
        let session = self.adopt(view, cx);
        self.open_sessions.test_main = Some(session.clone());
        session
    }

    /// The session `install_main_for_test` prepared, once.
    pub(crate) fn take_test_main(
        &mut self,
    ) -> Option<gpui_kit::Entity<crate::session::open::OpenSession>> {
        self.open_sessions.test_main.take()
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

/// A connected agent with a model catalog: Anthropic (two), OpenAI (three),
/// Google (one), then `bulk` models of the provider `bulk`.
#[cfg(test)]
pub(crate) fn models_fixture(bulk: usize) -> AgentsStore {
    let mut models = vec![
        json!({ "id": "anthropic/claude-sonnet-4-5", "label": "Claude Sonnet 4.5", "provider": "anthropic" }),
        json!({ "id": "anthropic/claude-haiku-4-5", "label": "Claude Haiku 4.5", "provider": "anthropic" }),
        json!({ "id": "openai/gpt-5", "label": "GPT-5", "provider": "openai" }),
        json!({ "id": "openai/gpt-5-mini", "label": "GPT-5 mini", "provider": "openai" }),
        json!({ "id": "openai/o3", "label": "o3", "provider": "openai" }),
        json!({ "id": "google/gemini-2.5-pro", "label": "Gemini 2.5 Pro", "provider": "google" }),
    ];
    models.extend((0..bulk).map(|i| {
        json!({ "id": format!("bulk/m{i:02}"), "label": format!("Bulk {i:02}"), "provider": "bulk" })
    }));
    let mut store = super::automations::schedule_ui_fixture();
    let rt = store.runtime.get_mut("test-agent").expect("fixture agent");
    rt.models = super::parse_models(&json!({ "models": models }));
    rt.models_loaded = true;
    store
}

/// One WebSocket agent `test-agent` at `address` (`host:port`), not connected
/// yet, and nothing read from or written to the user's saved state (unlike
/// `AgentsStore::new`). The film spike seeds sessions and models from it.
#[cfg(test)]
pub(crate) fn websocket_fixture(address: &str) -> AgentsStore {
    let (address, secure) = super::normalize_address(address).expect("a host:port address");
    AgentsStore {
        configs: vec![super::AgentConfig {
            id: "test-agent".into(),
            name: address.clone(),
            endpoint: super::Endpoint::WebSocket { address, secure },
            auto_start: false,
            host_managed: false,
        }],
        prefs: Default::default(),
        launch: Default::default(),
        open_sessions: Default::default(),
        requests: Vec::new(),
        runtime: Default::default(),
        sidebar: Default::default(),
        early_commands: Default::default(),
        #[cfg(not(target_family = "wasm"))]
        updates: Default::default(),
        first_run: false,
    }
}

/// Append `models` (wire shape: `id`, `label`, `provider`) to the fixture
/// agent's catalog, after the ones it has.
#[cfg(test)]
pub(crate) fn add_models_for_test(store: &mut AgentsStore, models: Value) {
    let rt = store.runtime.get_mut("test-agent").expect("fixture agent");
    rt.models
        .extend(super::parse_models(&json!({ "models": models })));
}

/// The fixture agent's mesh: `(id, label)` per node.
#[cfg(test)]
pub(crate) fn set_mesh_nodes_for_test(store: &mut AgentsStore, nodes: &[(&str, &str)]) {
    let rt = store.runtime.get_mut("test-agent").expect("fixture agent");
    rt.mesh_nodes = nodes
        .iter()
        .map(|(id, label)| super::MeshNode {
            id: (*id).into(),
            label: (*label).into(),
            active_sessions: 0,
            capabilities: vec![],
            transport: "iroh".into(),
        })
        .collect();
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
        let task = cx.update(|cx| {
            store.update(cx, |s, cx| {
                let placement = s.placement();
                s.launch(request("test-agent", "/work", "hi", placement), cx)
            })
        });
        cx.run_until_parked();
        let first = serde_json::from_str(&requests.try_recv().unwrap()).unwrap();
        drop(task);
        first
    }

    fn request(
        agent: &str,
        cwd: &str,
        prompt: &str,
        placement: super::Placement,
    ) -> super::LaunchRequest {
        super::LaunchRequest {
            agent_id: agent.into(),
            cwd: cwd.into(),
            prompt: prompt.into(),
            attachments: Vec::new(),
            placement,
            model: None,
            reasoning: None,
            mode: None,
        }
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
        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.launch.target = Some("thor-id".into());
                let placement = s.placement();
                let progress = super::LaunchProgress {
                    busy: true,
                    creating_on: s.creating_on("test-agent", &placement),
                    ..Default::default()
                };
                assert_eq!(progress.label(), "Creating session on thor…");
                // On the agent's own machine there is no peer to name.
                assert_eq!(
                    s.creating_on("test-agent", &super::Placement::local()),
                    None
                );
                assert_eq!(
                    super::LaunchProgress::default().label(),
                    "Creating session…"
                );
            })
        });
    }

    #[gpui_kit::test]
    fn launch_checks_the_peer_profile_still_exists(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| super::peer_fixture());
        let (rpc, requests) = cx.update(|cx| store.update(cx, |s, _| s.rpc_for_test("test-agent")));
        let launch = |cx: &mut gpui_kit::TestAppContext| {
            let task = cx.update(|cx| {
                store.update(cx, |s, cx| {
                    s.launch.target = Some("thor-id".into());
                    s.launch.remote_profile = Some("coder".into());
                    let placement = s.placement();
                    s.launch(request("test-agent", "/w", "hi", placement), cx)
                })
            });
            cx.run_until_parked();
            task
        };
        let next = || -> Value { serde_json::from_str(&requests.try_recv().unwrap()).unwrap() };
        let reply = |request: &Value, profiles: Value| {
            rpc.handle(
                &json!({ "id": request["id"], "result": { "node_id": "thor-id", "profiles": profiles } })
                    .to_string(),
            );
        };

        // The peer dropped the profile: no session is created.
        let task = launch(cx);
        let check = next();
        assert_eq!(check["method"], "_querymt/profiles");
        assert_eq!(check["params"]["node_id"], "thor-id");
        reply(&check, json!([{ "id": "reviewer" }]));
        cx.run_until_parked();
        assert!(requests.try_recv().is_err());
        assert!(matches!(
            futures::FutureExt::now_or_never(task),
            Some(Err(super::LaunchError::PeerProfileGone))
        ));
        store.read_with(cx, |s, _| {
            // The Start options see the fresh list.
            let listed = &s.peer_profiles("test-agent", "thor-id").unwrap().profiles;
            assert_eq!(listed[0].id, "reviewer");
        });

        // Still there: the session is created with it.
        let _task = launch(cx);
        let check = next();
        reply(&check, json!([{ "id": "coder" }]));
        cx.run_until_parked();
        let create = next();
        assert_eq!(create["method"], "_querymt/remote/createSession");
        assert_eq!(create["params"]["profile_id"], "coder");
    }

    /// A first prompt (by launch) and a later one (by send) both go through
    /// `start_prompt`: each carries its `client_prompt_id` and each turn ends
    /// in `finish_prompt`, which clears `prompting`.
    #[gpui_kit::test]
    fn launch_and_send_start_prompts_the_same_way(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;

        use crate::session::open::Delivery;

        let store = cx.new(|_| crate::store::automations::schedule_ui_fixture());
        let (rpc, requests) = cx.update(|cx| store.update(cx, |s, _| s.rpc_for_test("test-agent")));

        // Answer everything but `session/prompt`, which is returned to the
        // test (its reply ends the turn).
        let pump = |cx: &mut gpui_kit::TestAppContext| -> Vec<Value> {
            let mut prompts = Vec::new();
            loop {
                cx.run_until_parked();
                let Ok(text) = requests.try_recv() else { break };
                let request: Value = serde_json::from_str(&text).unwrap();
                match request["method"].as_str() {
                    Some("session/prompt") => prompts.push(request),
                    Some("session/new") => {
                        let reply = json!({ "id": request["id"], "result": { "sessionId": "s1" } });
                        rpc.handle(&reply.to_string());
                    }
                    _ => {
                        let reply = json!({ "id": request["id"], "result": {} });
                        rpc.handle(&reply.to_string());
                    }
                }
            }
            prompts
        };
        let shown: std::rc::Rc<
            std::cell::RefCell<Option<gpui_kit::Entity<crate::session::open::OpenSession>>>,
        > = Default::default();
        let prompting = |cx: &mut gpui_kit::TestAppContext| {
            shown
                .borrow()
                .as_ref()
                .map(|session| session.read_with(cx, |o, _| o.view.prompting))
        };
        let prompt_id = |request: &Value| {
            let params = &request["params"];
            assert_eq!(params["sessionId"], "s1");
            params
                .pointer("/_meta/querymt/client_prompt_id")
                .and_then(Value::as_str)
                .expect("client_prompt_id")
                .to_owned()
        };
        let finish = |cx: &mut gpui_kit::TestAppContext, request: &Value| {
            let reply = json!({ "id": request["id"], "result": { "stopReason": "end_turn" } });
            rpc.handle(&reply.to_string());
            cx.run_until_parked();
        };

        let launched = cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.launch(
                    request("test-agent", "/w", "first", super::Placement::local()),
                    cx,
                )
            })
        });
        let first = pump(cx);
        // The caller gets the session and shows it.
        let session = futures::FutureExt::now_or_never(launched)
            .expect("launch finished")
            .expect("session");
        *shown.borrow_mut() = Some(session);
        assert_eq!(first.len(), 1);
        let first_id = prompt_id(&first[0]);
        assert_eq!(prompting(cx), Some(true));
        finish(cx, &first[0]);
        assert_eq!(prompting(cx), Some(false));

        cx.update(|cx| {
            let session = shown.borrow().clone().expect("shown");
            store.update(cx, |s, cx| {
                s.send_prompt(&session, "second".into(), Vec::new(), Delivery::Steer, cx)
            })
        });
        let second = pump(cx);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["method"], "session/prompt");
        let second_id = prompt_id(&second[0]);
        assert_ne!(first_id, second_id);
        assert_eq!(prompting(cx), Some(true));
        finish(cx, &second[0]);
        assert_eq!(prompting(cx), Some(false));
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
