use super::*;
use super::utils::ext_json_response;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_auth_status(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize, Default)]
        struct StatusReq {
            #[serde(default)]
            provider: Option<String>,
        }

        let parsed: StatusReq = serde_json::from_str(req.params.get()).unwrap_or_default();
        let statuses = self
            .oauth_service
            .auth_status(parsed.provider.as_deref())
            .await;
        ext_json_response(&serde_json::json!({ "providers": statuses }))
    }

    pub(super) async fn handle_ext_auth_start(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize)]
        struct StartReq {
            provider: String,
        }

        let parsed: StartReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;
        let result = self
            .oauth_service
            .start_flow("acp", &parsed.provider, None)
            .await
            .map_err(|e| Error::internal_error().data(serde_json::json!({"error": e})))?;
        ext_json_response(&result)
    }

    pub(super) async fn handle_ext_auth_complete(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize)]
        struct CompleteReq {
            flow_id: String,
            response: String,
        }

        let parsed: CompleteReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;
        let result = self
            .oauth_service
            .complete_flow("acp", &parsed.flow_id, &parsed.response)
            .await;
        ext_json_response(&result)
    }

    pub(super) async fn handle_ext_auth_logout(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize)]
        struct LogoutReq {
            provider: String,
        }

        let parsed: LogoutReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;
        let result = self.oauth_service.logout("acp", &parsed.provider).await;
        ext_json_response(&result)
    }
}
