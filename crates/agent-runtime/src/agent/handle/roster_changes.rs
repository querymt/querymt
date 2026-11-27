//! Changing the roster of a running root agent (design D9).
//!
//! The roster is rebuilt from configuration on every start; nothing here is
//! persisted. A change takes effect for the next turn of every session,
//! because the `delegate` tool renders its definition from the roster.

use super::LocalAgentHandle;
use crate::config::{DelegateConfig, RemoteAgentConfig};
use crate::delegation::member::{
    MemberBuildSpec, MemberSpec, available_local_tool_names, build_local_member,
};
use crate::delegation::roster::{
    AgentId, AgentRoster, Member, MemberAgent, MemberOrigin, RosterChange, RosterChangeKind,
    RosterError, RosterGeneration,
};
use std::sync::Arc;

/// The payload of `querymt/agents/changed` and of the replies to
/// `querymt/agents/add` and `querymt/agents/remove`.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RosterChanged<'a> {
    pub generation: RosterGeneration,
    /// The member this change added (`querymt/agents/changed` and the reply
    /// to `querymt/agents/add`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<&'a str>,
    /// The member this change removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<&'a str>,
}

impl<'a> RosterChanged<'a> {
    pub(super) fn of(change: &'a RosterChange, profile_id: Option<&'a str>) -> Self {
        let id = Some(change.id.as_str());
        Self {
            generation: change.generation,
            added: id.filter(|_| change.kind == RosterChangeKind::Added),
            removed: id.filter(|_| change.kind == RosterChangeKind::Removed),
            profile_id,
        }
    }
}

impl LocalAgentHandle {
    /// Adds a member to this root's roster and announces the change.
    ///
    /// Fails, leaving the roster unchanged and sending nothing, when
    /// delegation is disabled, the ID is blank or taken, the description is
    /// not acceptable from outside the process, `peer` is set, or the member
    /// cannot be built.
    pub async fn add_roster_member(
        &self,
        spec: MemberSpec,
    ) -> Result<RosterGeneration, RosterError> {
        let change = self.apply_roster_add(spec)?;
        self.after_roster_change(&change, None).await;
        Ok(change.generation)
    }

    /// Adds a member described by an already lowered [`MemberBuildSpec`],
    /// for in-process callers such as the closure builder. Same rules and
    /// announcement as [`Self::add_roster_member`], except that nothing here
    /// came from outside the process, so no external checks apply.
    pub async fn add_roster_spec(
        &self,
        spec: MemberBuildSpec,
    ) -> Result<RosterGeneration, RosterError> {
        let roster = self
            .config
            .roster()
            .filter(|roster| roster.delegation_enabled())
            .ok_or(RosterError::DelegationDisabled)?;
        if let Some(existing) = roster.snapshot().get(spec.id.as_str()) {
            return Err(RosterError::DuplicateId {
                id: spec.id.clone(),
                origin: existing.origin(),
            });
        }
        let member = Self::build_runtime_member(roster, &spec)?;
        let change = roster.add_change(member)?;
        self.after_roster_change(&change, None).await;
        Ok(change.generation)
    }

    fn build_runtime_member(
        roster: &Arc<AgentRoster>,
        spec: &MemberBuildSpec,
    ) -> Result<Member, RosterError> {
        let handle = build_local_member(spec, roster.context()).map_err(|source| {
            match source.downcast::<RosterError>() {
                Ok(error) => error,
                Err(source) => RosterError::Build {
                    id: spec.id.clone(),
                    source,
                },
            }
        })?;
        Ok(Member::built(
            spec.info(),
            MemberOrigin::Runtime,
            MemberAgent::Local(handle),
        ))
    }

    /// Removes a member, whatever its origin, and announces the change. A
    /// delegation that already holds the member keeps it and finishes.
    pub async fn remove_roster_member(&self, id: &str) -> Result<RosterGeneration, RosterError> {
        let change = self.apply_roster_remove(id)?;
        self.after_roster_change(&change, None).await;
        Ok(change.generation)
    }

    /// Adds the member without announcing it. `querymt/agents/add` uses this
    /// for a profile's runtime, because the announcement must come from the
    /// handle that serves the client and carry the profile ID.
    pub(crate) fn apply_roster_add(&self, spec: MemberSpec) -> Result<RosterChange, RosterError> {
        let roster = self
            .config
            .roster()
            .filter(|roster| roster.delegation_enabled())
            .ok_or(RosterError::DelegationDisabled)?;
        let id = AgentId::try_from(spec.raw_id())?;
        if let Some(existing) = roster.snapshot().get(id.as_str()) {
            return Err(RosterError::DuplicateId {
                id,
                origin: existing.origin(),
            });
        }
        spec.check_external(&id)?;
        let member = match spec {
            MemberSpec::Local(delegate) => self.local_runtime_member(roster, &id, *delegate)?,
            MemberSpec::Remote(remote) => self.remote_runtime_member(id, remote)?,
        };
        roster.add_change(member)
    }

    /// Removes the member without announcing it; see [`Self::apply_roster_add`].
    pub(crate) fn apply_roster_remove(&self, id: &str) -> Result<RosterChange, RosterError> {
        let roster = self
            .config
            .roster()
            .filter(|roster| roster.delegation_enabled())
            .ok_or(RosterError::DelegationDisabled)?;
        roster.remove_change(id)
    }

    fn local_runtime_member(
        &self,
        roster: &Arc<AgentRoster>,
        id: &AgentId,
        delegate: DelegateConfig,
    ) -> Result<Member, RosterError> {
        if delegate.peer.is_some() {
            return Err(RosterError::PeerRoutingUnsupported);
        }
        let build_error = |source: anyhow::Error| {
            // A refusal the roster itself raised stays typed.
            match source.downcast::<RosterError>() {
                Ok(error) => error,
                Err(source) => RosterError::Build {
                    id: id.clone(),
                    source,
                },
            }
        };
        // Selectors resolve against this runtime's MCP servers and the
        // member's own, exactly as for a configured delegate. The context
        // gives the new member the runtime's event stream and mesh.
        let spec = MemberBuildSpec::from_delegate_config(
            delegate,
            &self.config.mcp_servers,
            &available_local_tool_names(),
        )
        .map_err(build_error)?;
        Self::build_runtime_member(roster, &spec)
    }

    /// A remote member is listed without contacting its peer; the node is
    /// resolved when a delegation needs it.
    fn remote_runtime_member(
        &self,
        id: AgentId,
        remote: RemoteAgentConfig,
    ) -> Result<Member, RosterError> {
        #[cfg(feature = "remote")]
        {
            use crate::delegation::AgentInfo;
            use crate::delegation::roster::RemoteMember;

            if self.mesh().is_none() {
                return Err(RosterError::MeshRequired);
            }
            let info = AgentInfo {
                isolation: Default::default(),
                id,
                name: remote.name,
                description: remote.description,
                capabilities: remote.capabilities,
                required_capabilities: vec![],
                meta: None,
            };
            Ok(Member::built(
                info,
                MemberOrigin::Runtime,
                MemberAgent::Remote(RemoteMember::new(remote.peer, None)),
            ))
        }
        #[cfg(not(feature = "remote"))]
        {
            let _ = (id, remote);
            Err(RosterError::MeshRequired)
        }
    }

    /// Sends `querymt/agents/changed` to every connected client: the
    /// broadcast reaches transports that subscribe, and the bridge reaches
    /// the stdio connection, as for schedule changes.
    /// What follows every successful roster change, from whichever path made
    /// it: the client notification, then the `subagent_added` /
    /// `subagent_removed` hooks of the runtime that changed. Hooks only
    /// observe; a failure is logged and never undoes the change.
    pub(crate) async fn after_roster_change(
        &self,
        change: &RosterChange,
        profile_id: Option<&str>,
    ) {
        self.announce_roster_change(change, profile_id).await;
        self.run_subagent_change_hooks(change, profile_id).await;
    }

    pub(super) async fn run_subagent_change_hooks(
        &self,
        change: &RosterChange,
        profile_id: Option<&str>,
    ) {
        use crate::hooks::{HookEventConfig, SubagentChangeRequest};

        let event = match change.kind {
            RosterChangeKind::Added => HookEventConfig::SubagentAdded,
            RosterChangeKind::Removed => HookEventConfig::SubagentRemoved,
        };
        let label = event.label();
        let request = SubagentChangeRequest {
            event,
            subagent_id: change.id.to_string(),
            name: change.name.clone(),
            description: change.description.clone(),
            capabilities: change.capabilities.clone(),
            origin: change.origin.to_string(),
            location: match change.location {
                crate::delegation::roster::MemberLocation::Local => "local",
                crate::delegation::roster::MemberLocation::Remote => "remote",
            }
            .to_string(),
            generation: change.generation.get(),
            profile_id: profile_id.map(str::to_string),
            cwd: None,
        };
        match self.config.hooks.run_subagent_change(request).await {
            Ok(result) => {
                for notice in result.notices {
                    if notice.is_error {
                        log::warn!("{label} hook for '{}': {}", change.id, notice.message);
                    } else {
                        log::info!("{label} hook for '{}': {}", change.id, notice.message);
                    }
                }
            }
            Err(error) => log::warn!("{label} hook for '{}' failed: {error}", change.id),
        }
    }

    pub(crate) async fn announce_roster_change(
        &self,
        change: &RosterChange,
        profile_id: Option<&str>,
    ) {
        self.notify_all_clients(
            crate::acp::shared::QMT_NOTIFICATION_AGENTS_CHANGED,
            &RosterChanged::of(change, profile_id),
        )
        .await;
    }
}
