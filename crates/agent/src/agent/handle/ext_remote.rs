#[cfg(feature = "remote")]
use super::utils::ext_json_response;
use super::*;

fn default_attach() -> bool {
    true
}

impl LocalAgentHandle {
    pub(super) async fn handle_ext_remote_sessions(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RemoteSessionsReq {
            node_id: String,
            #[serde(default)]
            offset: Option<u32>,
            #[serde(default)]
            limit: Option<u32>,
        }

        let parsed: RemoteSessionsReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let nm_ref = self.find_node_manager(&parsed.node_id).await?;
            let response = self
                .list_remote_sessions(&nm_ref, parsed.offset, parsed.limit)
                .await?;
            ext_json_response(&serde_json::json!({
                "nodeId": parsed.node_id,
                "sessions": response.sessions,
                "nextOffset": response.next_offset,
                "totalCount": response.total_count,
            }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_remote_create_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CreateReq {
            node_id: String,
            #[serde(default)]
            cwd: Option<String>,
            #[serde(default = "default_attach")]
            attach: bool,
        }

        let parsed: CreateReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let nm_ref = self.find_node_manager(&parsed.node_id).await?;
            let resp = self
                .create_remote_session(&nm_ref, parsed.cwd.clone())
                .await?;

            if !parsed.attach {
                return ext_json_response(&serde_json::json!({
                    "sessionId": resp.session_id,
                    "nodeId": parsed.node_id,
                    "attached": false,
                    "configOptions": [],
                }));
            }

            let snapshot = self
                .attach_remote_session_for_ext(
                    &parsed.node_id,
                    &resp.session_id,
                    Some(resp.handoff),
                )
                .await?;

            ext_json_response(&serde_json::json!({
                "sessionId": resp.session_id,
                "nodeId": parsed.node_id,
                "attached": true,
                "configOptions": [],
                "snapshot": snapshot,
            }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_remote_attach_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AttachReq {
            node_id: String,
            session_id: String,
        }

        let parsed: AttachReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let snapshot = self
                .attach_remote_session_for_ext(&parsed.node_id, &parsed.session_id, None)
                .await?;

            ext_json_response(&serde_json::json!({
                "sessionId": parsed.session_id,
                "nodeId": parsed.node_id,
                "attached": true,
                "configOptions": [],
                "snapshot": snapshot,
            }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }

    #[cfg(feature = "remote")]
    async fn attach_remote_session_for_ext(
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
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DismissReq {
            session_id: String,
        }

        let parsed: DismissReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            {
                let mut registry = self.registry.lock().await;
                registry.detach_remote_session(&parsed.session_id).await;
            }

            self.config
                .provider
                .history_store()
                .remove_remote_session_bookmark(&parsed.session_id)
                .await
                .map_err(|e| {
                    Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
                })?;

            ext_json_response(&serde_json::json!({ "success": true }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }
}
