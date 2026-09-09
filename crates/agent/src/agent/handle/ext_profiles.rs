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
                "configured_default_model_id": Self::configured_delegate_model_id(&runtime.agent().handle(), &agent.id),
            })
        }));

        ext_json_response(&serde_json::json!({
            "profile_id": profile_id,
            "agents": agents,
        }))
    }

    /// Read current assignments without loading a session actor or applying client preferences.
    pub(super) async fn handle_ext_delegate_models(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        use crate::control::delegate_models::{
            DELEGATE_MODELS_VERSION, DelegateAssignmentInfo, DelegateAssignmentSource,
            DelegateAssignmentsInfo, DelegateModelsRequest, OrphanedDelegateAssignment,
        };

        let parsed: DelegateModelsRequest = serde_json::from_str(req.params.get())
            .map_err(|error| Error::invalid_params().data(error.to_string()))?;
        let session_id = parsed.session_id.trim();
        if session_id.is_empty() {
            return Err(Error::invalid_params().data("session_id must be nonempty"));
        }
        let profiles = self.profiles().ok_or_else(Error::invalid_params)?;
        let binding = profiles.session_binding(session_id).await.ok_or_else(|| {
            Error::invalid_params().data("session is not bound to an available profile")
        })?;
        let runtime = profiles
            .runtime_for_profile(&binding.profile_id)
            .await
            .map_err(|error| {
                Error::internal_error().data(format_prefixed_error_chain(
                    "Failed to load bound profile",
                    &error,
                ))
            })?;
        let handle = runtime.agent().handle();
        let store = handle.config.provider.history_store();
        let session = store
            .get_session(session_id)
            .await
            .map_err(Error::into_internal_error)?
            .ok_or_else(|| Error::invalid_params().data("unknown session"))?;
        let state = store
            .get_delegate_assignments(session_id)
            .await
            .map_err(Error::into_internal_error)?;
        let editable = binding.agent_id.is_none()
            && session.fork_origin != Some(crate::session::domain::ForkOrigin::Delegation);
        let mut agents = if editable {
            handle.agent_registry().list_agents()
        } else {
            Vec::new()
        };
        agents.sort_by(|left, right| left.id.cmp(&right.id));
        let mut assignments = Vec::with_capacity(agents.len());
        for agent in agents {
            let model = match &state {
                Some(state) => state.overrides.get(&agent.id).cloned(),
                None => {
                    handle
                        .config
                        .delegate_model_overrides
                        .get(session_id, &agent.id)
                        .await
                }
            };
            assignments.push(DelegateAssignmentInfo {
                agent_id: agent.id.clone(),
                name: agent.name,
                description: agent.description,
                source: if model.is_some() {
                    DelegateAssignmentSource::Override
                } else {
                    DelegateAssignmentSource::ProfileDefault
                },
                model: model.into(),
                configured_default_model_id: Self::configured_delegate_model_id(&handle, &agent.id)
                    .into(),
            });
        }
        // Keep orphan overrides visible after a profile edit; never silently turn them into inheritance.
        let orphaned_overrides = match &state {
            Some(state) => state
                .overrides
                .iter()
                .filter(|(id, _)| handle.agent_registry().get_agent(id).is_none())
                .map(|(id, model)| OrphanedDelegateAssignment {
                    agent_id: id.clone(),
                    model: model.clone(),
                })
                .collect(),
            None => handle
                .config
                .delegate_model_overrides
                .list_parent(session_id)
                .await
                .into_iter()
                .filter(|(id, _)| handle.agent_registry().get_agent(id).is_none())
                .map(|(agent_id, model)| OrphanedDelegateAssignment { agent_id, model })
                .collect(),
        };
        ext_json_response(&DelegateAssignmentsInfo {
            version: DELEGATE_MODELS_VERSION,
            session_id: session_id.to_owned(),
            profile_id: binding.profile_id,
            revision: state.as_ref().map(|state| state.revision).into(),
            durable: state.is_some(),
            editable,
            assignments,
            orphaned_overrides,
        })
    }

    // This is profile configuration, not a resolved Mesh route or historical execution identity.
    fn configured_delegate_model_id(handle: &LocalAgentHandle, agent_id: &str) -> Option<String> {
        let target = handle.agent_registry().get_handle(agent_id)?;
        let local = target.as_any().downcast_ref::<LocalAgentHandle>()?;
        let config = local.config.provider.initial_config();
        Some(format!(
            "{}/{}",
            config.provider.as_deref()?,
            config.model.as_deref()?
        ))
    }

    pub(super) async fn handle_ext_set_delegate_model(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        use crate::control::delegate_models::{
            DELEGATE_MODELS_VERSION, SetDelegateModelRequest, SetDelegateModelResponse,
        };

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
        let store = profile_handle.config.provider.history_store();
        let session = store
            .get_session(session_id)
            .await
            .map_err(Error::into_internal_error)?
            .ok_or_else(|| Error::invalid_params().data("unknown session for bound profile"))?;
        if binding.agent_id.is_some()
            || session.fork_origin == Some(crate::session::domain::ForkOrigin::Delegation)
        {
            return Err(
                Error::invalid_params().data("Configure delegate models on their parent session")
            );
        }
        let before = store
            .get_delegate_assignments(session_id)
            .await
            .map_err(Error::into_internal_error)?;
        if before.is_none() && parsed.expected_revision.is_some() {
            return Err(Error::invalid_params().data(
                "This storage backend does not support revision-checked delegate assignments",
            ));
        }
        if profile_handle
            .agent_registry()
            .get_agent(agent_id)
            .is_none()
            && !(parsed.model_id.0.is_none()
                && match &before {
                    Some(state) => state.overrides.contains_key(agent_id),
                    None => profile_handle
                        .config
                        .delegate_model_overrides
                        .get(session_id, agent_id)
                        .await
                        .is_some(),
                })
        {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown delegate agent",
                "sessionId": session_id,
                "agentId": agent_id,
                "profileId": binding.profile_id,
            })));
        }

        let model = match parsed.model_id.0 {
            Some(model_id) => {
                let model_id = model_id.trim();
                if model_id.is_empty() {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "model_id must be null or a non-empty string",
                    })));
                }
                let node_id = parsed.node_id.as_deref().map(str::trim).map(str::to_string);
                if node_id.as_ref().is_some_and(String::is_empty) {
                    return Err(
                        Error::invalid_params().data("node_id must be null or a non-empty string")
                    );
                }
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
                let current_delegate_model =
                    Self::configured_delegate_model_id(&profile_handle, agent_id);
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

        let persisted = if before.is_some() {
            Some(
                store
                    .set_delegate_assignment(
                        session_id,
                        agent_id,
                        model.clone(),
                        parsed.expected_revision,
                    )
                    .await
                    .map_err(|error| {
                        match error {
                        crate::session::error::SessionError::DelegateAssignmentRevisionConflict {
                            expected,
                            found,
                        } => Error::new(
                            crate::control::delegate_models::DELEGATE_ASSIGNMENT_CONFLICT_ACP_CODE,
                            "Delegate assignments changed; refresh before retrying",
                        )
                        .data(serde_json::json!({
                            "code": "delegate_assignment_conflict",
                            "expected_revision": expected,
                            "actual_revision": found,
                            "message": "Delegate assignments changed; refresh before retrying",
                        })),
                        other => Error::into_internal_error(other),
                    }
                    })?,
            )
        } else {
            None
        };
        // The legacy cache is only used by storage backends without durable support.
        let legacy_changed = if persisted.is_none() {
            profile_handle
                .config
                .delegate_model_overrides
                .update(session_id, agent_id, model.clone())
                .await
        } else {
            false
        };
        let revision = persisted.as_ref().map(|write| write.assignments.revision);
        if legacy_changed || persisted.as_ref().is_some_and(|write| write.changed) {
            profile_handle.emit_event(
                session_id,
                AgentEventKind::DelegateModelsChanged { revision },
            );
        }
        ext_json_response(&SetDelegateModelResponse {
            version: DELEGATE_MODELS_VERSION,
            session_id: session_id.to_owned(),
            agent_id: agent_id.to_owned(),
            model: model.into(),
            revision: revision.into(),
            durable: persisted.is_some(),
        })
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
