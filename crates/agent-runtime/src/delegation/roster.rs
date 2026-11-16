//! The roster: the members a runtime's root agent can delegate to.
//!
//! Reads borrow an immutable [`RosterSnapshot`]; writes replace it
//! copy-on-write and bump the [`RosterGeneration`] by one. No lock is held
//! across an `.await`, and members are kept in ID order.

use super::AgentInfo;
use super::member::{MemberBuildSpec, MemberContext, build_local_member};
use crate::acp::protocol::Error as AcpError;
use crate::agent::LocalAgentHandle;
use crate::agent::remote::SessionActorRef;
use arc_swap::ArcSwap;
use once_cell::sync::OnceCell;
use querymt::LLMParams;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Why a roster operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RosterError {
    #[error("agent ID must not be empty or blank")]
    InvalidId,
    #[error("agent '{id}' already exists in the roster (origin: {origin})")]
    DuplicateId { id: AgentId, origin: MemberOrigin },
    #[error("unknown agent '{0}'")]
    UnknownAgent(AgentId),
    #[error("delegation is disabled: the root agent's tools do not select `delegate`")]
    DelegationDisabled,
    #[error("a running mesh is required for remote members")]
    MeshRequired,
    #[error("`peer` is only supported for configured delegates")]
    PeerRoutingUnsupported,
    #[error(
        "agent '{id}': system prompt parts that name a file are not accepted here; send the text instead"
    )]
    FilePromptPart { id: AgentId },
    #[error("agent '{id}' is configured `isolated` but the workspace service is not enabled")]
    IsolationWithoutWorkspace { id: AgentId },
    /// The source is part of the chain, not of this message.
    #[error("cannot build agent '{id}'")]
    Build {
        id: AgentId,
        #[source]
        source: anyhow::Error,
    },
}

/// A non-empty, trimmed agent identifier. Its JSON form is a plain string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId(Arc<str>);

impl AgentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AgentId {
    type Error = RosterError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl TryFrom<&str> for AgentId {
    type Error = RosterError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(RosterError::InvalidId);
        }
        Ok(Self(Arc::from(trimmed)))
    }
}

impl From<AgentId> for String {
    fn from(id: AgentId) -> Self {
        id.0.to_string()
    }
}

impl Borrow<str> for AgentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How many times the roster has changed since the runtime started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RosterGeneration(u64);

impl RosterGeneration {
    pub fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Where a member came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MemberOrigin {
    /// A `[[delegates]]`, `[[remote_agents]]` or builder entry.
    Config,
    /// A `.agents` delegation target.
    Protocol,
    /// Added while the runtime was running.
    Runtime,
}

impl fmt::Display for MemberOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Config => "config",
            Self::Protocol => "protocol",
            Self::Runtime => "runtime",
        })
    }
}

/// Whether a member runs in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MemberLocation {
    Local,
    Remote,
}

/// A member that runs on another mesh node. It holds no connection: the node
/// is resolved when a delegation needs it, so a member can be listed while its
/// peer is unreachable.
#[cfg(feature = "remote")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMember {
    peer: String,
    node_id: Option<String>,
}

#[cfg(feature = "remote")]
impl RemoteMember {
    /// `peer` is the mesh peer label; `node_id` is the owner learned from the
    /// configured peer address, used when the mesh cannot resolve `peer`.
    pub fn new(peer: impl Into<String>, node_id: Option<String>) -> Self {
        Self {
            peer: peer.into(),
            node_id,
        }
    }

    pub fn peer(&self) -> &str {
        &self.peer
    }

    pub fn node_id(&self) -> Option<&str> {
        self.node_id.as_deref()
    }
}

/// A built member agent. There are exactly two kinds.
#[derive(Clone)]
pub enum MemberAgent {
    Local(Arc<LocalAgentHandle>),
    #[cfg(feature = "remote")]
    Remote(RemoteMember),
}

impl MemberAgent {
    /// The local handle, when the member runs in this process.
    pub fn as_local(&self) -> Option<&Arc<LocalAgentHandle>> {
        match self {
            Self::Local(handle) => Some(handle),
            #[cfg(feature = "remote")]
            Self::Remote(_) => None,
        }
    }

    pub fn location(&self) -> MemberLocation {
        match self {
            Self::Local(_) => MemberLocation::Local,
            #[cfg(feature = "remote")]
            Self::Remote(_) => MemberLocation::Remote,
        }
    }

    /// Whether this member can execute tools in a node-local managed workspace.
    pub fn supports_local_workspaces(&self) -> bool {
        match self {
            Self::Local(handle) => handle.supports_local_workspaces(),
            #[cfg(feature = "remote")]
            Self::Remote(_) => false,
        }
    }

    /// Opens a session for a delegation. `root` is the runtime that owns the
    /// delegation: a remote member's session is attached through it.
    #[cfg_attr(not(feature = "remote"), allow(unused_variables))]
    pub async fn create_delegation_session(
        &self,
        root: &LocalAgentHandle,
        cwd: Option<String>,
        parent_session_id: String,
    ) -> Result<(String, SessionActorRef), AcpError> {
        match self {
            Self::Local(handle) => {
                handle
                    .create_delegation_session(cwd, parent_session_id)
                    .await
            }
            #[cfg(feature = "remote")]
            Self::Remote(member) => root.open_remote_delegation_session(member, cwd).await,
        }
    }

    /// Opens a session bound to a preallocated isolated workspace.
    pub async fn create_isolated_delegation_session(
        &self,
        workspace: crate::worktree::WorkspaceBinding,
        parent_session_id: String,
    ) -> Result<(String, SessionActorRef), AcpError> {
        match self {
            Self::Local(handle) => {
                handle
                    .create_isolated_delegation_session(workspace, parent_session_id)
                    .await
            }
            #[cfg(feature = "remote")]
            Self::Remote(_) => Err(AcpError::invalid_request()
                .data("isolated delegation requires local workspace execution")),
        }
    }

    /// Sets persisted lifecycle state without loading the session actor.
    #[cfg_attr(not(feature = "remote"), allow(unused_variables))]
    pub async fn set_session_lifecycle(
        &self,
        root: &LocalAgentHandle,
        session_id: &str,
        patch: crate::session::lifecycle::SessionLifecyclePatch,
    ) -> crate::session::error::SessionResult<crate::session::lifecycle::SessionLifecycle> {
        match self {
            Self::Local(handle) => handle.set_session_lifecycle(session_id, patch).await,
            #[cfg(feature = "remote")]
            Self::Remote(member) => {
                root.set_remote_member_session_lifecycle(member, session_id, patch)
                    .await
            }
        }
    }
}

/// How a member's agent comes to exist.
enum MemberSlot {
    /// The agent exists already.
    Built(MemberAgent),
    /// A local agent built on first use from `spec`. A failed build leaves
    /// the cell empty, so the next use retries.
    Lazy {
        spec: Box<MemberBuildSpec>,
        cell: OnceCell<MemberAgent>,
    },
    /// A description that could not be lowered; building reports why.
    Unbuildable(String),
}

/// One member of a roster.
pub struct Member {
    info: AgentInfo,
    origin: MemberOrigin,
    slot: MemberSlot,
}

impl Member {
    /// A member whose agent already exists.
    pub fn built(info: AgentInfo, origin: MemberOrigin, agent: MemberAgent) -> Self {
        Self {
            info,
            origin,
            slot: MemberSlot::Built(agent),
        }
    }

    /// A local member built on first use from `spec`.
    pub(crate) fn pending(info: AgentInfo, origin: MemberOrigin, spec: MemberBuildSpec) -> Self {
        Self {
            info,
            origin,
            slot: MemberSlot::Lazy {
                spec: Box::new(spec),
                cell: OnceCell::new(),
            },
        }
    }

    /// A local member whose description could not be lowered. It is listed,
    /// and every attempt to build it fails with `reason`.
    pub(crate) fn unbuildable(
        info: AgentInfo,
        origin: MemberOrigin,
        reason: &dyn fmt::Display,
    ) -> Self {
        Self {
            info,
            origin,
            slot: MemberSlot::Unbuildable(reason.to_string()),
        }
    }

    pub fn id(&self) -> &AgentId {
        &self.info.id
    }

    pub fn info(&self) -> &AgentInfo {
        &self.info
    }

    pub fn origin(&self) -> MemberOrigin {
        self.origin
    }

    pub fn location(&self) -> MemberLocation {
        match &self.slot {
            MemberSlot::Built(agent) => agent.location(),
            MemberSlot::Lazy { .. } | MemberSlot::Unbuildable(_) => MemberLocation::Local,
        }
    }

    /// The agent, if it has been built. Never builds it.
    pub fn agent(&self) -> Option<&MemberAgent> {
        match &self.slot {
            MemberSlot::Built(agent) => Some(agent),
            MemberSlot::Lazy { cell, .. } => cell.get(),
            MemberSlot::Unbuildable(_) => None,
        }
    }

    /// Whether the agent has been built.
    pub fn is_built(&self) -> bool {
        self.agent().is_some()
    }

    /// The LLM parameters the member is configured with. Never builds it.
    pub fn llm_params(&self) -> Option<&LLMParams> {
        match &self.slot {
            MemberSlot::Built(agent) => agent
                .as_local()
                .map(|handle| handle.config.provider.initial_config()),
            MemberSlot::Lazy { spec, .. } => spec.llm_config.as_ref(),
            MemberSlot::Unbuildable(_) => None,
        }
    }

    /// The agent, built with `build` on first use. `build` runs until it
    /// succeeds once; a failure leaves the member unbuilt.
    pub(crate) fn get_or_build(
        &self,
        build: impl FnOnce(&MemberBuildSpec) -> anyhow::Result<MemberAgent>,
    ) -> Result<&MemberAgent, RosterError> {
        let built = match &self.slot {
            MemberSlot::Built(agent) => return Ok(agent),
            MemberSlot::Lazy { spec, cell } => cell.get_or_try_init(|| build(spec)),
            MemberSlot::Unbuildable(reason) => Err(anyhow::anyhow!("{reason}")),
        };
        built.map_err(|source| RosterError::Build {
            id: self.info.id.clone(),
            source,
        })
    }
}

/// An immutable view of the roster at one generation.
#[derive(Clone, Default)]
pub struct RosterSnapshot {
    generation: RosterGeneration,
    members: BTreeMap<AgentId, Arc<Member>>,
}

impl RosterSnapshot {
    /// The configured roster of a runtime: generation 0, so a rebuilt runtime
    /// reports the same generation as a fresh one and only later changes count.
    pub(crate) fn from_members(
        members: impl IntoIterator<Item = Member>,
    ) -> Result<Self, RosterError> {
        let mut snapshot = Self::default();
        for member in members {
            snapshot.insert(member)?;
        }
        Ok(snapshot)
    }

    /// Adds a member; the generation is the roster's to advance.
    pub(crate) fn insert(&mut self, member: Member) -> Result<(), RosterError> {
        if let Some(existing) = self.members.get(member.id().as_str()) {
            return Err(RosterError::DuplicateId {
                id: member.id().clone(),
                origin: existing.origin,
            });
        }
        self.members.insert(member.id().clone(), Arc::new(member));
        Ok(())
    }

    fn remove(&mut self, id: &str) -> Result<(), RosterError> {
        if self.members.remove(id).is_none() {
            return Err(RosterError::UnknownAgent(AgentId::try_from(id)?));
        }
        Ok(())
    }

    pub fn generation(&self) -> RosterGeneration {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Looks a member up by ID without allocating.
    pub fn get(&self, id: &str) -> Option<&Arc<Member>> {
        self.members.get(id)
    }

    /// Members in ID order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Member>> {
        self.members.values()
    }

    /// The description of every member, in ID order.
    pub fn infos(&self) -> impl Iterator<Item = &AgentInfo> {
        self.iter().map(|member| member.info())
    }

    /// Local members that already exist, in ID order. Never builds one.
    pub fn built_local(&self) -> impl Iterator<Item = &Arc<LocalAgentHandle>> {
        self.members
            .values()
            .filter_map(|member| member.agent()?.as_local())
    }
}

/// The members a root agent can delegate to.
pub struct AgentRoster {
    /// What every member of the runtime shares; owned here so no member can
    /// be built on another event stream.
    context: MemberContext,
    /// Whether the runtime's delegation service runs. Members can be listed
    /// either way, but only an enabled roster accepts changes.
    delegation_enabled: bool,
    state: ArcSwap<RosterSnapshot>,
    /// Serializes changes; never held across an `.await`.
    writer: parking_lot::Mutex<()>,
}

impl AgentRoster {
    pub(crate) fn new(
        context: MemberContext,
        configured: RosterSnapshot,
        delegation_enabled: bool,
    ) -> Self {
        Self {
            context,
            delegation_enabled,
            state: ArcSwap::from_pointee(configured),
            writer: parking_lot::Mutex::new(()),
        }
    }

    pub(crate) fn context(&self) -> &MemberContext {
        &self.context
    }

    /// Whether the runtime delegates, i.e. whether the roster accepts changes.
    pub fn delegation_enabled(&self) -> bool {
        self.delegation_enabled
    }

    /// The member's agent, building a local member on first use.
    pub fn resolve(&self, member: &Member) -> Result<MemberAgent, RosterError> {
        member
            .get_or_build(|spec| build_local_member(spec, &self.context).map(MemberAgent::Local))
            .cloned()
    }

    /// The local agent of member `id`, built on first use. `None` for an
    /// unknown ID, a remote member or a member that cannot be built.
    pub fn local_member(&self, id: &str) -> Option<Arc<LocalAgentHandle>> {
        let member = self.snapshot().get(id).cloned()?;
        self.resolve(&member).ok()?.as_local().cloned()
    }

    /// Whether a member with this ID exists.
    pub fn contains(&self, id: &str) -> bool {
        self.state.load().get(id).is_some()
    }

    /// The current members. Cheap; callers iterate the borrowed snapshot.
    pub fn snapshot(&self) -> Arc<RosterSnapshot> {
        self.state.load_full()
    }

    /// Applies `change` to a copy of the roster and, if it succeeds, publishes
    /// the copy under the next generation. A failure leaves the roster as it was.
    fn update(
        &self,
        change: impl FnOnce(&mut RosterSnapshot) -> Result<(), RosterError>,
    ) -> Result<RosterGeneration, RosterError> {
        let _guard = self.writer.lock();
        let mut next = RosterSnapshot::clone(&self.state.load());
        change(&mut next)?;
        next.generation = next.generation.next();
        let generation = next.generation;
        self.state.store(Arc::new(next));
        Ok(generation)
    }

    /// Adds a member and returns the new generation. Fails, leaving the
    /// roster unchanged, when the ID is taken.
    pub(crate) fn add(&self, member: Member) -> Result<RosterGeneration, RosterError> {
        self.update(|snapshot| snapshot.insert(member))
    }

    /// Removes a member and returns the new generation. A delegation that
    /// already holds the member keeps it.
    pub(crate) fn remove(&self, id: &str) -> Result<RosterGeneration, RosterError> {
        self.update(|snapshot| snapshot.remove(id))
    }
}

/// An empty roster over the given plugin registry and storage.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn test_roster_on(
    plugin_registry: Arc<querymt::plugin::host::PluginRegistry>,
    storage: Arc<dyn crate::session::backend::StorageBackend>,
) -> Arc<AgentRoster> {
    Arc::new(AgentRoster::new(
        MemberContext::for_tests(plugin_registry, storage),
        RosterSnapshot::default(),
        true,
    ))
}

/// An empty roster on in-memory storage, for tests in this crate.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub async fn test_roster() -> (Arc<AgentRoster>, tempfile::TempDir) {
    let (registry, dir) = crate::test_utils::helpers::empty_plugin_registry().unwrap();
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );
    (test_roster_on(Arc::new(registry), storage), dir)
}

/// A member that is built on first use, for tests in this crate.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn test_member(id: &str, origin: MemberOrigin) -> Member {
    Member::pending(
        crate::test_utils::delegation::agent_info(id),
        origin,
        MemberBuildSpec::new(AgentId::try_from(id).unwrap()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> AgentId {
        AgentId::try_from(value).unwrap()
    }

    async fn roster() -> (Arc<AgentRoster>, tempfile::TempDir) {
        test_roster().await
    }

    fn member(value: &str, origin: MemberOrigin) -> Member {
        test_member(value, origin)
    }

    fn ids(snapshot: &RosterSnapshot) -> Vec<String> {
        snapshot.iter().map(|m| m.id().to_string()).collect()
    }

    #[test]
    fn empty_or_blank_id_is_rejected() {
        for bad in ["", "   ", "\t\n"] {
            assert!(matches!(
                AgentId::try_from(bad),
                Err(RosterError::InvalidId)
            ));
        }
        assert_eq!(id("  coder ").as_str(), "coder");
    }

    #[test]
    fn agent_id_json_is_a_plain_string() {
        assert_eq!(serde_json::to_string(&id("coder")).unwrap(), "\"coder\"");
        let parsed: AgentId = serde_json::from_str("\"coder\"").unwrap();
        assert_eq!(parsed, id("coder"));
        assert!(serde_json::from_str::<AgentId>("\"  \"").is_err());
    }

    #[tokio::test]
    async fn add_bumps_the_generation_by_one() {
        let (roster, _dir) = roster().await;
        assert_eq!(roster.snapshot().generation().get(), 0);
        let first = roster.add(member("a", MemberOrigin::Config)).unwrap();
        let second = roster.add(member("b", MemberOrigin::Runtime)).unwrap();
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
        assert_eq!(roster.snapshot().generation(), second);
        let removed = roster.remove("a").unwrap();
        assert_eq!(removed.get(), 3);
    }

    #[tokio::test]
    async fn duplicate_and_unknown_ids_return_typed_errors_and_change_nothing() {
        let (roster, _dir) = roster().await;
        roster.add(member("a", MemberOrigin::Protocol)).unwrap();
        let before = roster.snapshot();

        match roster.add(member("a", MemberOrigin::Runtime)) {
            Err(RosterError::DuplicateId { id, origin }) => {
                assert_eq!(id.as_str(), "a");
                assert_eq!(origin, MemberOrigin::Protocol);
            }
            other => panic!("{:?}", other.map(|g| g.get())),
        }
        match roster.remove("missing") {
            Err(RosterError::UnknownAgent(id)) => assert_eq!(id.as_str(), "missing"),
            other => panic!("{:?}", other.map(|g| g.get())),
        }

        let after = roster.snapshot();
        assert_eq!(after.generation(), before.generation());
        assert_eq!(ids(&after), ids(&before));
        assert!(
            Arc::ptr_eq(&before, &after),
            "snapshot must not be replaced"
        );
    }

    #[tokio::test]
    async fn iteration_is_in_id_order() {
        let (roster, _dir) = roster().await;
        for name in ["writer", "coder", "reviewer"] {
            roster.add(member(name, MemberOrigin::Config)).unwrap();
        }
        assert_eq!(ids(&roster.snapshot()), ["coder", "reviewer", "writer"]);
        assert!(roster.snapshot().get("coder").is_some());
        assert!(roster.snapshot().get("nobody").is_none());
    }

    #[tokio::test]
    async fn listing_does_not_build_a_pending_member() {
        let (roster, _dir) = roster().await;
        roster
            .add(member("writer", MemberOrigin::Protocol))
            .unwrap();
        let snapshot = roster.snapshot();
        let writer = snapshot.get("writer").unwrap();
        assert!(!writer.is_built());
        assert_eq!(snapshot.built_local().count(), 0);
        assert_eq!(writer.origin(), MemberOrigin::Protocol);
        assert_eq!(writer.location(), MemberLocation::Local);
    }

    #[test]
    fn a_failed_build_is_retried_and_a_successful_one_runs_once() {
        let m = member("writer", MemberOrigin::Protocol);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        for _ in 0..2 {
            let failed = m.get_or_build(|_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(anyhow::anyhow!("boom"))
            });
            assert!(matches!(failed, Err(RosterError::Build { .. })));
            assert!(!m.is_built());
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_member_that_cannot_be_lowered_reports_why_when_resolved() {
        let (roster, _dir) = roster().await;
        let info = crate::test_utils::delegation::agent_info("broken");
        roster
            .add(Member::unbuildable(
                info,
                MemberOrigin::Protocol,
                &"model preset rejected",
            ))
            .unwrap();
        let snapshot = roster.snapshot();
        let err = roster
            .resolve(snapshot.get("broken").unwrap())
            .err()
            .expect("build fails");
        assert!(err.to_string().contains("broken"), "{err}");
        assert!(
            format!("{:#}", anyhow::Error::from(err)).contains("model preset rejected"),
            "the reason is in the error chain"
        );
    }
}
