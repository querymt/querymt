use super::utils::format_prefixed_error_chain;
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.set_session_model(req).await
    }

    pub(super) async fn handle_set_session_mode(
        &self,
        req: agent_client_protocol::schema::SetSessionModeRequest,
    ) -> Result<agent_client_protocol::schema::SetSessionModeResponse, Error> {
        let mode = req
            .mode_id
            .0
            .parse::<AgentMode>()
            .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e })))?;
        let session_id = req.session_id.to_string();

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.set_mode(mode).await.map_err(Error::from)?;
        Ok(agent_client_protocol::schema::SetSessionModeResponse::new())
    }

    pub(super) async fn handle_set_session_config_option(
        &self,
        req: agent_client_protocol::schema::SetSessionConfigOptionRequest,
    ) -> Result<agent_client_protocol::schema::SetSessionConfigOptionResponse, Error> {
        use agent_client_protocol::schema::SessionConfigOptionValue;

        let config_id = req.config_id.0.as_ref();

        let SessionConfigOptionValue::ValueId { value: value_id } = req.value else {
            return Err(Error::invalid_params().data(serde_json::json!({
                "error": "config option requires a value id",
            })));
        };

        let session_id = req.session_id.0.to_string();

        match config_id {
            "model" => {
                #[derive(serde::Deserialize)]
                struct QuerymtMeta {
                    #[serde(rename = "modelEntry")]
                    model_entry: crate::model_registry::ModelEntry,
                }

                #[derive(serde::Deserialize)]
                struct RequestMeta {
                    querymt: QuerymtMeta,
                }

                let model_id = value_id.0.to_string();
                #[cfg(feature = "remote")]
                let provider_node_id = req
                    .meta
                    .as_ref()
                    .and_then(|m| {
                        serde_json::from_value::<RequestMeta>(serde_json::Value::Object(m.clone()))
                            .ok()
                    })
                    .and_then(|m| m.querymt.model_entry.node_id)
                    .map(|node_id| {
                        crate::agent::remote::NodeId::parse(&node_id).map_err(|e| {
                            Error::invalid_params().data(serde_json::json!({
                                "error": format!("invalid modelEntry.node_id '{}': {}", node_id, e),
                            }))
                        })
                    })
                    .transpose()?;

                let session_ref = {
                    let registry = self.registry.lock().await;
                    registry.get(&session_id).cloned().ok_or_else(|| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": "unknown session",
                            "sessionId": session_id,
                        }))
                    })?
                };

                #[cfg(feature = "remote")]
                let msg = crate::agent::messages::SetSessionModel {
                    req: SetSessionModelRequest::new(session_id.clone(), model_id),
                    provider_node_id,
                };
                #[cfg(not(feature = "remote"))]
                let msg = crate::agent::messages::SetSessionModel {
                    req: SetSessionModelRequest::new(session_id.clone(), model_id),
                    provider_node_id: None,
                };

                session_ref.set_session_model_with_node(msg).await?;

                let mode = session_ref.get_mode().await.unwrap_or(AgentMode::Build);
                let effort = session_ref.get_reasoning_effort().await.ok().flatten();

                let config_options = self
                    .session_config_options(Some(&session_id), mode, effort)
                    .await?;
                Ok(
                    agent_client_protocol::schema::SetSessionConfigOptionResponse::new(
                        config_options,
                    ),
                )
            }
            "profile" => {
                let profile_id = value_id.0.to_string();
                let profiles = self.profiles().ok_or_else(|| {
                    Error::invalid_params().data(serde_json::json!({
                        "message": "profiles are not configured",
                    }))
                })?;
                let available_profiles = profiles.list_profiles().await.map_err(|err| {
                    Error::internal_error().data(serde_json::json!({
                        "message": format_prefixed_error_chain("Failed to list profiles", &err),
                    }))
                })?;
                if !available_profiles
                    .iter()
                    .any(|profile| profile.id == profile_id)
                {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": format!("unknown profile: {profile_id}"),
                        "profileId": profile_id,
                    })));
                }

                let Some(binding) = profiles.session_binding(&session_id).await else {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "cannot change profile for an existing unbound session",
                        "sessionId": session_id,
                    })));
                };
                if binding.profile_id != profile_id {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "cannot change profile for an existing session",
                        "sessionId": session_id,
                        "currentProfileId": binding.profile_id,
                        "requestedProfileId": profile_id,
                    })));
                }

                let session_ref = {
                    let registry = self.registry.lock().await;
                    registry.get(&session_id).cloned().ok_or_else(|| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": "unknown session",
                            "sessionId": session_id,
                        }))
                    })?
                };
                let mode = session_ref.get_mode().await.unwrap_or(AgentMode::Build);
                let effort = session_ref.get_reasoning_effort().await.ok().flatten();
                let config_options = self
                    .session_config_options(Some(&session_id), mode, effort)
                    .await?;
                Ok(
                    agent_client_protocol::schema::SetSessionConfigOptionResponse::new(
                        config_options,
                    ),
                )
            }
            "mode" => {
                let mode = value_id
                    .0
                    .parse::<AgentMode>()
                    .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e })))?;

                let session_ref = {
                    let registry = self.registry.lock().await;
                    registry.get(&session_id).cloned().ok_or_else(|| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": "unknown session",
                            "sessionId": session_id,
                        }))
                    })?
                };
                session_ref.set_mode(mode).await.map_err(Error::from)?;

                let effort = session_ref.get_reasoning_effort().await.ok().flatten();
                let config_options = self
                    .session_config_options(Some(&session_id), mode, effort)
                    .await?;
                Ok(
                    agent_client_protocol::schema::SetSessionConfigOptionResponse::new(
                        config_options,
                    ),
                )
            }
            "reasoning_effort" => {
                let effort_str = value_id.0.as_ref();
                let effort = if effort_str == "auto" {
                    None
                } else {
                    Some(
                        serde_json::from_value::<querymt::chat::ReasoningEffort>(
                            serde_json::json!(effort_str),
                        )
                        .map_err(|e| {
                            Error::invalid_params().data(serde_json::json!({
                            "error": format!("Invalid reasoning effort '{}': {}", effort_str, e),
                        }))
                        })?,
                    )
                };

                let session_ref = {
                    let registry = self.registry.lock().await;
                    registry.get(&session_id).cloned().ok_or_else(|| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": "unknown session",
                            "sessionId": session_id,
                        }))
                    })?
                };

                session_ref
                    .set_reasoning_effort(effort)
                    .await
                    .map_err(Error::from)?;

                let mode = session_ref.get_mode().await.unwrap_or(AgentMode::Build);
                let config_options = self
                    .session_config_options(Some(&session_id), mode, effort)
                    .await?;

                Ok(
                    agent_client_protocol::schema::SetSessionConfigOptionResponse::new(
                        config_options,
                    ),
                )
            }
            _ => Err(Error::invalid_params().data(serde_json::json!({
                "error": format!("Unsupported configId: {}", config_id),
            }))),
        }
    }
}
