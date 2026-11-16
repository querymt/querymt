//! `querymt/agents/list|add|remove`: reading and changing the roster over ACP
//! (design D9). Changes last for the life of the runtime.

use super::roster_changes::RosterChanged;
use super::utils::{ext_json_response, format_error_chain, format_prefixed_error_chain};
use super::*;
use crate::delegation::member::MemberSpec;
use crate::delegation::roster::{MemberLocation, MemberOrigin, RosterError, RosterGeneration};

#[derive(serde::Deserialize, Default)]
struct ListRequest {
    #[serde(default, alias = "profileId")]
    profile_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct AddRequest {
    #[serde(default, alias = "profileId")]
    profile_id: Option<String>,
    agent: MemberSpec,
}

#[derive(serde::Deserialize)]
struct RemoveRequest {
    #[serde(default, alias = "profileId")]
    profile_id: Option<String>,
    id: String,
}

#[derive(serde::Serialize)]
struct AgentEntry<'a> {
    id: &'a str,
    name: &'a str,
    description: &'a str,
    capabilities: &'a [String],
    origin: MemberOrigin,
    location: MemberLocation,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentList<'a> {
    generation: RosterGeneration,
    agents: Vec<AgentEntry<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_id: Option<&'a str>,
}

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::invalid_params().data(serde_json::json!({ "message": message.to_string() }))
}

/// A refusal of the request is the client's to fix; a member that fails to
/// build is the runtime's.
fn roster_error(error: RosterError) -> Error {
    match error {
        RosterError::Build { .. } => Error::internal_error().data(serde_json::json!({
            "message": format_error_chain(&anyhow::Error::from(error)),
        })),
        error => invalid(error),
    }
}

fn parse<T: serde::de::DeserializeOwned>(req: &ExtRequest, what: &str) -> Result<T, Error> {
    serde_json::from_str(req.params.get())
        .map_err(|error| invalid(format!("invalid {what}: {error}")))
}

/// The runtime a roster request addresses: the profile's, or the serving root.
struct RosterTarget {
    profile: Option<(Arc<LocalAgentHandle>, String)>,
}

impl RosterTarget {
    fn handle<'a>(&'a self, root: &'a LocalAgentHandle) -> &'a LocalAgentHandle {
        self.profile
            .as_ref()
            .map_or(root, |(handle, _)| handle.as_ref())
    }

    fn profile_id(&self) -> Option<&str> {
        self.profile.as_ref().map(|(_, id)| id.as_str())
    }
}

impl LocalAgentHandle {
    /// The handle whose roster a request addresses: a profile's runtime with
    /// `profileId`, otherwise this handle.
    async fn roster_target(&self, profile_id: Option<&str>) -> Result<RosterTarget, Error> {
        let Some(profile_id) = profile_id.map(str::trim) else {
            return Ok(RosterTarget { profile: None });
        };
        if profile_id.is_empty() {
            return Err(invalid("profileId must be a non-empty string"));
        }
        let profiles = self
            .profiles()
            .ok_or_else(|| invalid("profiles are not configured"))?;
        let runtime = profiles
            .runtime_for_profile(profile_id)
            .await
            .map_err(|error| {
                invalid(format!(
                    "{} (profile '{profile_id}')",
                    format_prefixed_error_chain("Failed to load profile", &error)
                ))
            })?;
        Ok(RosterTarget {
            profile: Some((runtime.handle(), profile_id.to_string())),
        })
    }

    pub(super) async fn handle_ext_agents_list(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: Option<ListRequest> = parse(&req, "agents/list params")?;
        let requested = parsed.and_then(|request| request.profile_id);
        let target = self.roster_target(requested.as_deref()).await?;
        let snapshot = target
            .handle(self)
            .config
            .roster()
            .map(|roster| roster.snapshot())
            .unwrap_or_default();
        let agents = snapshot
            .iter()
            .map(|member| {
                let info = member.info();
                AgentEntry {
                    id: info.id.as_str(),
                    name: &info.name,
                    description: &info.description,
                    capabilities: &info.capabilities,
                    origin: member.origin(),
                    location: member.location(),
                }
            })
            .collect();
        ext_json_response(&AgentList {
            generation: snapshot.generation(),
            agents,
            profile_id: target.profile_id(),
        })
    }

    pub(super) async fn handle_ext_agents_add(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: AddRequest = parse(&req, "agents/add params")?;
        let target = self.roster_target(parsed.profile_id.as_deref()).await?;
        let change = target
            .handle(self)
            .apply_roster_add(parsed.agent)
            .map_err(roster_error)?;
        // The hooks that run are the changed runtime's own; the
        // announcement is sent by the handle that serves the client.
        self.announce_roster_change(&change, target.profile_id())
            .await;
        target
            .handle(self)
            .run_subagent_change_hooks(&change, target.profile_id())
            .await;
        ext_json_response(&RosterChanged::of(&change, target.profile_id()))
    }

    pub(super) async fn handle_ext_agents_remove(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: RemoveRequest = parse(&req, "agents/remove params")?;
        let target = self.roster_target(parsed.profile_id.as_deref()).await?;
        let change = target
            .handle(self)
            .apply_roster_remove(&parsed.id)
            .map_err(roster_error)?;
        self.announce_roster_change(&change, target.profile_id())
            .await;
        target
            .handle(self)
            .run_subagent_change_hooks(&change, target.profile_id())
            .await;
        ext_json_response(&RosterChanged::of(&change, target.profile_id()))
    }
}
