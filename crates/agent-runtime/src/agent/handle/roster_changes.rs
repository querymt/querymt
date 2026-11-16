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
    AgentId, AgentRoster, Member, MemberAgent, MemberOrigin, RosterError, RosterGeneration,
};
use std::sync::Arc;

/// The payload of `querymt/agents/changed` and of the replies to
/// `querymt/agents/add` and `querymt/agents/remove`.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RosterChanged<'a> {
    pub generation: RosterGeneration,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<&'a str>,
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
        let generation = self.apply_roster_add(spec)?;
        self.announce_roster_change(generation, None).await;
        Ok(generation)
    }

    /// Removes a member, whatever its origin, and announces the change. A
    /// delegation that already holds the member keeps it and finishes.
    pub async fn remove_roster_member(&self, id: &str) -> Result<RosterGeneration, RosterError> {
        let generation = self.apply_roster_remove(id)?;
        self.announce_roster_change(generation, None).await;
        Ok(generation)
    }

    /// Adds the member without announcing it. `querymt/agents/add` uses this
    /// for a profile's runtime, because the announcement must come from the
    /// handle that serves the client and carry the profile ID.
    pub(crate) fn apply_roster_add(
        &self,
        spec: MemberSpec,
    ) -> Result<RosterGeneration, RosterError> {
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
            MemberSpec::Local(delegate) => self.local_runtime_member(roster, id, *delegate)?,
            MemberSpec::Remote(remote) => self.remote_runtime_member(id, remote)?,
        };
        roster.add(member)
    }

    /// Removes the member without announcing it; see [`Self::apply_roster_add`].
    pub(crate) fn apply_roster_remove(&self, id: &str) -> Result<RosterGeneration, RosterError> {
        let roster = self
            .config
            .roster()
            .filter(|roster| roster.delegation_enabled())
            .ok_or(RosterError::DelegationDisabled)?;
        roster.remove(id)
    }

    fn local_runtime_member(
        &self,
        roster: &Arc<AgentRoster>,
        id: AgentId,
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
        let handle = build_local_member(&spec, roster.context()).map_err(build_error)?;
        Ok(Member::built(
            spec.info(),
            MemberOrigin::Runtime,
            MemberAgent::Local(handle),
        ))
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
    pub(crate) async fn announce_roster_change(
        &self,
        generation: RosterGeneration,
        profile_id: Option<&str>,
    ) {
        let params = RosterChanged {
            generation,
            profile_id,
        };
        let raw = match serde_json::value::to_raw_value(&params) {
            Ok(raw) => raw,
            Err(error) => {
                log::warn!("Failed to encode agents-changed notification: {error}");
                return;
            }
        };
        let notification = crate::acp::protocol::ExtNotification::new(
            crate::acp::shared::QMT_NOTIFICATION_AGENTS_CHANGED,
            Arc::from(raw),
        );
        self.broadcast_ext_notification(notification.clone());

        let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
        if let Some(bridge) = bridge
            && let Err(error) = bridge.notify_ext(notification).await
        {
            log::warn!("Failed to send agents-changed notification: {error}");
        }
    }
}
