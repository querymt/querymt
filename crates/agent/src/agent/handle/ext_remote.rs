#[cfg(feature = "remote")]
use super::utils::ext_json_response;
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_remote_sessions(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::RemoteSessionsRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::list_remote_sessions(self, parsed).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_remote_create_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::CreateRemoteSessionRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::create_remote_session(self, parsed).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_remote_attach_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::AttachRemoteSessionRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::attach_remote_session(self, parsed).await?;
        ext_json_response(&response)
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn attach_remote_session_for_ext(
        &self,
        node_id: &str,
        session_id: &str,
        handoff: Option<crate::agent::remote::node_manager::SessionHandoff>,
    ) -> Result<serde_json::Value, Error> {
        let (remote_ref, matched_scope) = match handoff {
            Some(handoff) => (self.resolve_handoff(session_id, handoff).await?, None),
            None => {
                let mesh = self
                    .mesh()
                    .ok_or_else(|| Error::invalid_request().data("mesh not bootstrapped"))?;
                let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
                let mut remote_ref = None;
                let mut matched_scope = None;
                let mut lookup_err = None;
                for scope in runtime.active_scopes() {
                    let dht_name = crate::agent::remote::scope::scoped_session(&scope, session_id);
                    match runtime
                        .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                        .await
                    {
                        Ok(Some(found)) => {
                            remote_ref = Some(found);
                            matched_scope = Some(scope);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => lookup_err = Some(e),
                    }
                }
                match remote_ref {
                    Some(found) => (found, matched_scope),
                    None => {
                        if let Some(err) = lookup_err {
                            log::debug!(
                                "remote attach scoped lookup error before resume fallback: {}",
                                err
                            );
                        }
                        let nm_ref = self.find_node_manager(node_id).await?;
                        let resumed = self
                            .resume_remote_session(&nm_ref, session_id.to_string())
                            .await?;
                        (
                            self.resolve_handoff(session_id, resumed.handoff).await?,
                            None,
                        )
                    }
                }
            }
        };

        let peer_label = self
            .list_remote_nodes()
            .await
            .into_iter()
            .find(|n| n.node_id.to_string() == node_id)
            .map(|n| n.hostname)
            .unwrap_or_else(|| node_id.to_string());

        self.attach_remote_session(
            session_id.to_string(),
            remote_ref,
            peer_label,
            matched_scope,
            Some(node_id.to_string()),
        )
        .await;

        let attached_session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned()
        }
        .ok_or_else(|| {
            Error::internal_error().data(format!(
                "Attached remote session '{}' but it is missing from local registry",
                session_id
            ))
        })?;

        attached_session_ref.get_mode().await.map_err(|e| {
            Error::internal_error().data(format!(
                "Remote session '{}' attached but failed health check on node '{}': {}",
                session_id, node_id, e
            ))
        })?;

        self.build_remote_attach_snapshot(session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.message))
    }

    pub(super) async fn handle_ext_remote_dismiss_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::DismissRemoteSessionRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::dismiss_remote_session(self, parsed).await?;
        ext_json_response(&response)
    }
}
