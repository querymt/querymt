use super::utils::{ext_json_response, format_prefixed_error_chain};
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_profiles(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize, Default)]
        struct Request {
            #[serde(alias = "nodeId")]
            node_id: Option<String>,
        }
        let parsed: Option<Request> = serde_json::from_str(req.params.get())
            .map_err(|error| Error::invalid_params().data(error.to_string()))?;
        if let Some(node_id) = parsed.and_then(|request| request.node_id) {
            #[cfg(feature = "remote")]
            {
                return ext_json_response(&self.remote_profiles(&node_id).await?);
            }
            #[cfg(not(feature = "remote"))]
            {
                let _ = node_id;
                return Err(Error::method_not_found());
            }
        }
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

        let delegates: Vec<crate::delegation::AgentInfo> = runtime
            .handle()
            .config
            .roster()
            .map(|roster| roster.snapshot().infos().cloned().collect())
            .unwrap_or_default();
        let mut agents = vec![serde_json::json!({
            "id": "primary",
            "name": "Session",
            "description": "Main profile agent",
            "capabilities": [],
        })];
        agents.extend(delegates.into_iter().map(|agent| {
            serde_json::json!({
                "id": agent.id.as_str(),
                "name": agent.name,
                "description": agent.description,
                "capabilities": agent.capabilities,
                "configured_default_model_id": Self::configured_delegate_model_id(&runtime.handle(), agent.id.as_str()),
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
        use crate::control::delegate_models::DelegateModelsRequest;

        let parsed: DelegateModelsRequest = serde_json::from_str(req.params.get())
            .map_err(|error| Error::invalid_params().data(error.to_string()))?;
        #[cfg(feature = "remote")]
        if let Some(node_id) = self.delegate_session_owner(&parsed.session_id).await? {
            return ext_json_response(&self.remote_delegate_models(&node_id, parsed).await?);
        }
        ext_json_response(&Self::owner_delegate_models(self.profiles().as_ref(), parsed).await?)
    }

    pub(crate) async fn owner_delegate_models(
        profiles: Option<&ProfileRuntime>,
        parsed: crate::control::delegate_models::DelegateModelsRequest,
    ) -> Result<crate::control::delegate_models::DelegateAssignmentsInfo, Error> {
        use crate::control::delegate_models::*;
        let session_id = parsed.session_id.trim();
        if session_id.is_empty() {
            return Err(Error::invalid_params().data("session_id must be nonempty"));
        }
        let profiles = profiles.ok_or_else(Error::invalid_params)?;
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
        let handle = runtime.handle();
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
        let agents: Vec<crate::delegation::AgentInfo> = if editable {
            handle
                .config
                .roster()
                .map(|roster| roster.snapshot().infos().cloned().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let legacy_routes = if state.is_none() {
            Some(
                handle
                    .config
                    .delegate_model_overrides
                    .list_parent_routes(session_id)
                    .await,
            )
        } else {
            None
        };
        let mut assignments = Vec::with_capacity(agents.len());
        for agent in agents {
            let route = match &state {
                Some(state) => crate::delegation::DelegateRouteOverrides {
                    model: state.overrides.get(agent.id.as_str()).cloned(),
                    reasoning_effort: state.reasoning_overrides.get(agent.id.as_str()).copied(),
                },
                None => legacy_routes
                    .as_ref()
                    .and_then(|routes| routes.get(agent.id.as_str()))
                    .cloned()
                    .unwrap_or_default(),
            };
            let model = route.model;
            let reasoning_effort = route.reasoning_effort;
            assignments.push(DelegateAssignmentInfo {
                agent_id: agent.id.to_string(),
                name: agent.name,
                description: agent.description,
                source: if model.is_some() {
                    DelegateAssignmentSource::Override
                } else {
                    DelegateAssignmentSource::ProfileDefault
                },
                model: model.into(),
                configured_default_model_id: Self::configured_delegate_model_id(
                    &handle,
                    agent.id.as_str(),
                )
                .into(),
                reasoning_effort: reasoning_effort.into(),
            });
        }
        // Keep orphan overrides visible after a profile edit; never silently turn them into inheritance.
        let routes = match &state {
            Some(state) => state
                .overrides
                .keys()
                .chain(state.reasoning_overrides.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|agent_id| {
                    (
                        agent_id.clone(),
                        crate::delegation::DelegateRouteOverrides {
                            model: state.overrides.get(&agent_id).cloned(),
                            reasoning_effort: state.reasoning_overrides.get(&agent_id).copied(),
                        },
                    )
                })
                .collect(),
            None => legacy_routes.unwrap_or_default(),
        };
        let orphaned_overrides = routes
            .into_iter()
            .filter(|(agent_id, _)| !Self::roster_has(&handle, agent_id))
            .map(|(agent_id, route)| OrphanedDelegateAssignment {
                model: route.model.into(),
                reasoning_effort: route.reasoning_effort.into(),
                agent_id,
            })
            .collect();
        Ok(DelegateAssignmentsInfo {
            version: DELEGATE_MODELS_VERSION,
            reasoning_effort_supported: true,
            session_id: session_id.to_owned(),
            profile_id: binding.profile_id,
            revision: state.as_ref().map(|state| state.revision).into(),
            durable: state.is_some(),
            editable,
            assignments,
            orphaned_overrides,
        })
    }

    fn roster_has(handle: &LocalAgentHandle, agent_id: &str) -> bool {
        handle
            .config
            .roster()
            .is_some_and(|roster| roster.contains(agent_id))
    }

    // This is profile configuration, not a resolved Mesh route or historical execution identity.
    fn configured_delegate_model_id(handle: &LocalAgentHandle, agent_id: &str) -> Option<String> {
        // Reads the configuration; listing a member never builds it.
        let snapshot = handle.config.roster()?.snapshot();
        let config = snapshot.get(agent_id)?.llm_params()?;
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
        use crate::control::delegate_models::SetDelegateModelRequest;

        let parsed: SetDelegateModelRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({
                    "message": format!("invalid setDelegateModel params: {e}"),
                }))
            })?;
        #[cfg(feature = "remote")]
        if let Some(node_id) = self.delegate_session_owner(&parsed.session_id).await? {
            return ext_json_response(&self.remote_set_delegate_model(&node_id, parsed).await?);
        }
        ext_json_response(
            &Self::owner_set_delegate_model(self.profiles().as_ref(), self.mesh_node_id(), parsed)
                .await?,
        )
    }

    pub(crate) async fn owner_set_delegate_model(
        profiles: Option<&ProfileRuntime>,
        owner_node_id: Option<String>,
        parsed: crate::control::delegate_models::SetDelegateModelRequest,
    ) -> Result<crate::control::delegate_models::SetDelegateModelResponse, Error> {
        use crate::control::delegate_models::*;
        let session_id = parsed.session_id.trim();
        let agent_id = parsed.agent_id.trim();
        if session_id.is_empty() || agent_id.is_empty() {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "session_id and agent_id must be non-empty strings",
            })));
        }

        let profiles = profiles.ok_or_else(|| {
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
        let profile_handle = runtime.handle();
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
        let orphan_route = match &before {
            Some(state) => crate::delegation::DelegateRouteOverrides {
                model: state.overrides.get(agent_id).cloned(),
                reasoning_effort: state.reasoning_overrides.get(agent_id).copied(),
            },
            None => {
                profile_handle
                    .config
                    .delegate_model_overrides
                    .get_route(session_id, agent_id)
                    .await
            }
        };
        let stored_orphan = orphan_route.model.is_some() || orphan_route.reasoning_effort.is_some();
        let clears_orphan = parsed.model_id.0.is_none()
            && match &parsed.reasoning_effort {
                crate::control::delegate_models::OptionalNullable::Missing => {
                    orphan_route.reasoning_effort.is_none()
                }
                crate::control::delegate_models::OptionalNullable::Null => true,
                crate::control::delegate_models::OptionalNullable::Value(_) => false,
            };
        if !Self::roster_has(&profile_handle, agent_id) && !(stored_orphan && clears_orphan) {
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
                let node_id = node_id.filter(|node| Some(node) != owner_node_id.as_ref());
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
                    .set_delegate_assignment_with_reasoning(
                        session_id,
                        agent_id,
                        model.clone(),
                        parsed.reasoning_effort.as_update(),
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
        let legacy_write = if persisted.is_none() {
            Some(
                profile_handle
                    .config
                    .delegate_model_overrides
                    .update_route(
                        session_id,
                        agent_id,
                        model.clone(),
                        parsed.reasoning_effort.as_update(),
                    )
                    .await,
            )
        } else {
            None
        };
        let legacy_changed = legacy_write.as_ref().is_some_and(|(changed, _)| *changed);
        let reasoning_effort = match (&persisted, &legacy_write) {
            (Some(write), _) => write.assignments.reasoning_overrides.get(agent_id).copied(),
            (None, Some((_, route))) => route.reasoning_effort,
            (None, None) => None,
        };
        let revision = persisted.as_ref().map(|write| write.assignments.revision);
        if legacy_changed || persisted.as_ref().is_some_and(|write| write.changed) {
            profile_handle.emit_event(
                session_id,
                AgentEventKind::DelegateModelsChanged { revision },
            );
        }
        Ok(SetDelegateModelResponse {
            version: DELEGATE_MODELS_VERSION,
            reasoning_effort_supported: true,
            session_id: session_id.to_owned(),
            agent_id: agent_id.to_owned(),
            model: model.into(),
            reasoning_effort: reasoning_effort.into(),
            revision: revision.into(),
            durable: persisted.is_some(),
        })
    }

    fn mesh_node_id(&self) -> Option<String> {
        #[cfg(feature = "remote")]
        {
            self.mesh().map(|mesh| mesh.peer_id().to_string())
        }
        #[cfg(not(feature = "remote"))]
        {
            None
        }
    }

    async fn profiles_response(&self) -> Result<serde_json::Value, Error> {
        Self::owner_profiles(self.profiles().as_ref()).await
    }

    pub(crate) async fn owner_profiles(
        profiles: Option<&ProfileRuntime>,
    ) -> Result<serde_json::Value, Error> {
        let Some(profiles) = profiles else {
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
