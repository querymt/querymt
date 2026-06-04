use crate::agent::agent_config::AgentConfig;
use crate::session::provider::ProviderRequest;
use async_trait::async_trait;
use querymt::LLMProvider;
use querymt_remote::{
    ProviderBuildRequest, ProviderHostActor, RemoteProviderBackend, RemoteProviderHostError,
    params_for_remote_provider,
};
use std::sync::Arc;

struct AgentConfigProviderBackend {
    config: Arc<AgentConfig>,
}

#[async_trait]
impl RemoteProviderBackend for AgentConfigProviderBackend {
    type Error = RemoteProviderHostError;

    fn host_default_params(&self, _provider_name: &str, _model: &str) -> Option<serde_json::Value> {
        params_for_remote_provider(self.config.provider.initial_params())
    }

    async fn build_provider(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
        self.config
            .provider
            .build_provider(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref()),
            )
            .await
            .map_err(|e| {
                RemoteProviderHostError::Internal(format!(
                    "ProviderHostActor: failed to build provider '{}' model '{}': {}",
                    request.provider_name, request.model, e
                ))
            })
    }
}

pub fn provider_backend_from_config(
    config: Arc<AgentConfig>,
) -> Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>> {
    Arc::new(AgentConfigProviderBackend { config })
}

pub fn provider_host_from_config(config: Arc<AgentConfig>) -> ProviderHostActor {
    ProviderHostActor::new(provider_backend_from_config(config))
}
