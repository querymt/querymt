use super::utils::{ext_json_response, format_prefixed_error_chain};
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_profiles(&self) -> Result<ExtResponse, Error> {
        ext_json_response(&self.profiles_response().await?)
    }

    pub(super) async fn handle_ext_set_active_profile(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize)]
        struct SetActiveProfileRequest {
            #[serde(alias = "profileId")]
            profile_id: String,
        }

        let parsed: SetActiveProfileRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({
                    "message": format!("invalid profile setActive params: {e}"),
                }))
            })?;
        let profile_id = parsed.profile_id.trim();
        if profile_id.is_empty() {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "profile_id must be a non-empty string",
            })));
        }

        let profiles = self.profiles().ok_or_else(|| {
            Error::invalid_params().data(serde_json::json!({
                "message": "profiles are not configured",
            }))
        })?;

        // This mutates the profile manager's shared backend default for all clients and
        // only affects sessions created after the change; existing sessions stay bound.
        profiles
            .set_active_profile(profile_id)
            .await
            .map_err(|err| {
                Error::invalid_params().data(serde_json::json!({
                    "message": format_prefixed_error_chain("Failed to set active profile", &err),
                    "profileId": profile_id,
                }))
            })?;

        ext_json_response(&self.profiles_response().await?)
    }

    pub(super) async fn handle_ext_profile_agents(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize)]
        struct ProfileAgentsRequest {
            #[serde(alias = "profileId")]
            profile_id: String,
        }

        let parsed: ProfileAgentsRequest = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({
                "message": format!("invalid profile agents params: {e}"),
            }))
        })?;
        let profile_id = parsed.profile_id.trim();
        if profile_id.is_empty() {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "profile_id must be a non-empty string",
            })));
        }

        let profiles = self.profiles().ok_or_else(|| {
            Error::invalid_params().data(serde_json::json!({
                "message": "profiles are not configured",
            }))
        })?;
        let runtime = profiles
            .runtime_for_profile(profile_id)
            .await
            .map_err(|err| {
                Error::invalid_params().data(serde_json::json!({
                    "message": format_prefixed_error_chain("Failed to load profile", &err),
                    "profileId": profile_id,
                }))
            })?;

        let mut delegates = runtime.agent().handle().agent_registry().list_agents();
        delegates.sort_by(|left, right| left.id.cmp(&right.id));
        let mut agents = vec![serde_json::json!({
            "id": "primary",
            "name": "Session",
            "description": "Main profile agent",
            "capabilities": [],
        })];
        agents.extend(delegates.into_iter().map(|agent| {
            serde_json::json!({
                "id": agent.id,
                "name": agent.name,
                "description": agent.description,
                "capabilities": agent.capabilities,
            })
        }));

        ext_json_response(&serde_json::json!({
            "profile_id": profile_id,
            "agents": agents,
        }))
    }

    pub(super) async fn handle_ext_set_delegate_model(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize)]
        struct SetDelegateModelRequest {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(alias = "agentId")]
            agent_id: String,
            #[serde(default, alias = "modelId")]
            model_id: Option<String>,
            #[serde(default, alias = "nodeId")]
            node_id: Option<String>,
        }

        let parsed: SetDelegateModelRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({
                    "message": format!("invalid setDelegateModel params: {e}"),
                }))
            })?;
        let session_id = parsed.session_id.trim();
        let agent_id = parsed.agent_id.trim();
        if session_id.is_empty() || agent_id.is_empty() {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "session_id and agent_id must be non-empty strings",
            })));
        }

        let profiles = self.profiles().ok_or_else(|| {
            Error::invalid_params().data(serde_json::json!({
                "message": "profiles are not configured",
            }))
        })?;
        let binding = profiles.session_binding(session_id).await.ok_or_else(|| {
            Error::invalid_params().data(serde_json::json!({
                "message": "session is not bound to a profile",
                "sessionId": session_id,
            }))
        })?;
        let runtime = profiles
            .runtime_for_profile(&binding.profile_id)
            .await
            .map_err(|err| {
                Error::internal_error().data(serde_json::json!({
                    "message": format_prefixed_error_chain("Failed to load bound profile", &err),
                    "profileId": binding.profile_id,
                    "sessionId": session_id,
                }))
            })?;
        let profile_handle = runtime.agent().handle();
        if profile_handle
            .registry
            .lock()
            .await
            .get(session_id)
            .is_none()
        {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown session for bound profile",
                "sessionId": session_id,
                "profileId": binding.profile_id,
            })));
        }
        if profile_handle
            .agent_registry()
            .get_agent(agent_id)
            .is_none()
        {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown delegate agent",
                "sessionId": session_id,
                "agentId": agent_id,
                "profileId": binding.profile_id,
            })));
        }

        let model = match parsed.model_id {
            Some(model_id) => {
                let model_id = model_id.trim();
                if model_id.is_empty() {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "model_id must be null or a non-empty string",
                    })));
                }
                let node_id = parsed
                    .node_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                #[cfg(not(feature = "remote"))]
                if node_id.is_some() {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "node_id requires the remote feature",
                    })));
                }

                let mut models = profile_handle.model_inventory.get_all_models().await;
                if models.is_empty() {
                    let refresh = profile_handle.model_inventory.trigger_refresh().await;
                    refresh.wait().await;
                    models = profile_handle.model_inventory.get_all_models().await;
                }
                if models.is_empty() {
                    models =
                        crate::model_registry::enumerate_local_models(&profile_handle.config).await;
                }
                let model_exists = models
                    .iter()
                    .any(|entry| entry.id == model_id && entry.node_id == node_id);
                let current_delegate_model = profile_handle
                    .agent_registry()
                    .get_handle(agent_id)
                    .and_then(|handle| {
                        handle
                            .as_any()
                            .downcast_ref::<LocalAgentHandle>()
                            .and_then(|handle| {
                                let config = handle.config.provider.initial_config();
                                Some(format!(
                                    "{}/{}",
                                    config.provider.as_deref()?,
                                    config.model.as_deref()?
                                ))
                            })
                    });
                if !model_exists
                    && (node_id.is_some() || current_delegate_model.as_deref() != Some(model_id))
                {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "unknown model or provider node",
                        "modelId": model_id,
                        "nodeId": node_id,
                    })));
                }

                Some(crate::delegation::DelegateModelOverride {
                    model_id: model_id.to_string(),
                    node_id,
                })
            }
            None => {
                if parsed.node_id.is_some() {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "node_id cannot be set when model_id is null",
                    })));
                }
                None
            }
        };

        if let Some(model) = model.clone() {
            profile_handle
                .config
                .delegate_model_overrides
                .set(session_id, agent_id, model)
                .await;
        } else {
            profile_handle
                .config
                .delegate_model_overrides
                .clear(session_id, agent_id)
                .await;
        }

        ext_json_response(&serde_json::json!({
            "session_id": session_id,
            "agent_id": agent_id,
            "model": model,
        }))
    }

    async fn profiles_response(&self) -> Result<serde_json::Value, Error> {
        let Some(profiles) = self.profiles() else {
            return Ok(serde_json::json!({
                "profiles": [],
                "active_profile_id": serde_json::Value::Null,
            }));
        };

        let profile_infos: Vec<serde_json::Value> = profiles
            .list_profiles()
            .await
            .map_err(|err| {
                Error::internal_error().data(serde_json::json!({
                    "message": format_prefixed_error_chain("Failed to list profiles", &err),
                }))
            })?
            .into_iter()
            .map(|metadata| {
                serde_json::json!({
                    "id": metadata.id,
                    "name": metadata.name,
                    "description": metadata.description,
                    "tags": metadata.tags,
                    "config_kind": metadata.config_kind.map(|kind| kind.storage_label()),
                    "source": metadata.source.storage_label(),
                    "fingerprint": metadata.fingerprint,
                })
            })
            .collect();
        let active_profile_id = profiles.active_profile_id().await;

        Ok(serde_json::json!({
            "profiles": profile_infos,
            "active_profile_id": active_profile_id,
        }))
    }
}
