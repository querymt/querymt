use crate::{
    ProviderBuildRequest, ProviderCatalogBackend, ProviderCatalogEntry, ProviderCatalogNodeInfo,
    ProviderCatalogSnapshot, RemoteProviderBackend, RemoteProviderHostError,
};
use async_trait::async_trait;
use querymt::LLMProvider;
use querymt::builder::LLMBuilder;
use querymt::plugin::host::PluginRegistry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub struct StaticCatalogBackend {
    snapshot: ProviderCatalogSnapshot,
}

impl StaticCatalogBackend {
    pub fn new(snapshot: ProviderCatalogSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn provider(
        node_id: impl Into<String>,
        node_label: Option<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self::providers(node_id, node_label, [provider.into()])
    }

    pub fn providers(
        node_id: impl Into<String>,
        node_label: Option<String>,
        providers: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            snapshot: ProviderCatalogSnapshot {
                node: ProviderCatalogNodeInfo {
                    node_id: node_id.into(),
                    node_label,
                    capabilities: vec!["provider-sharing".to_string()],
                },
                providers: providers
                    .into_iter()
                    .map(|provider| ProviderCatalogEntry {
                        provider,
                        model: None,
                        label: None,
                        family: None,
                        quant: None,
                    })
                    .collect(),
            },
        }
    }

    pub fn provider_model(
        node_id: impl Into<String>,
        node_label: Option<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::provider_models(node_id, node_label, provider, [model.into()])
    }

    pub fn provider_models(
        node_id: impl Into<String>,
        node_label: Option<String>,
        provider: impl Into<String>,
        models: impl IntoIterator<Item = String>,
    ) -> Self {
        let provider = provider.into();
        Self {
            snapshot: ProviderCatalogSnapshot {
                node: ProviderCatalogNodeInfo {
                    node_id: node_id.into(),
                    node_label,
                    capabilities: vec!["provider-sharing".to_string()],
                },
                providers: models
                    .into_iter()
                    .map(|model| ProviderCatalogEntry {
                        provider: provider.clone(),
                        model: Some(model),
                        label: None,
                        family: None,
                        quant: None,
                    })
                    .collect(),
            },
        }
    }
}

impl ProviderCatalogBackend for StaticCatalogBackend {
    fn snapshot(&self) -> ProviderCatalogSnapshot {
        self.snapshot.clone()
    }
}

pub struct ClosureProviderBackend<F> {
    build: F,
}

impl<F> ClosureProviderBackend<F> {
    pub fn new(build: F) -> Self {
        Self { build }
    }
}

#[async_trait]
impl<F, Fut> RemoteProviderBackend for ClosureProviderBackend<F>
where
    F: Send + Sync + Fn(ProviderBuildRequest) -> Fut,
    Fut: std::future::Future<Output = Result<Arc<dyn LLMProvider>, RemoteProviderHostError>> + Send,
{
    type Error = RemoteProviderHostError;

    async fn build_provider(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
        (self.build)(request).await
    }
}

pub struct StaticProviderBackend {
    provider: Arc<dyn LLMProvider>,
}

impl StaticProviderBackend {
    pub fn new(provider: Arc<dyn LLMProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl RemoteProviderBackend for StaticProviderBackend {
    type Error = RemoteProviderHostError;

    async fn build_provider(
        &self,
        _request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
        Ok(Arc::clone(&self.provider))
    }
}

pub struct RegistryProviderBackend {
    registry: Arc<PluginRegistry>,
}

impl RegistryProviderBackend {
    pub fn from_registry(registry: PluginRegistry) -> Self {
        Self {
            registry: Arc::new(registry),
        }
    }

    pub fn new(registry: Arc<PluginRegistry>) -> Self {
        Self { registry }
    }

    pub fn with_dynamic_loaders(self) -> Self {
        // querymt-remote depends on querymt's runtime surface, not its desktop-only
        // dynamic plugin loader helpers. Keep this as a compatibility no-op so
        // provider-only hosts can opt into static registration instead.
        self
    }

    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self, querymt::error::LLMError> {
        let registry = PluginRegistry::from_path(path)?;
        Ok(Self::from_registry(registry))
    }

    pub async fn load_all_plugins(&self) {
        self.registry.load_all_plugins().await;
    }

    pub fn register_static_http(&self, factory: Arc<dyn querymt::plugin::HTTPLLMProviderFactory>) {
        self.registry.register_static_http(factory);
    }
}

#[async_trait]
impl RemoteProviderBackend for RegistryProviderBackend {
    type Error = RemoteProviderHostError;

    async fn build_provider(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
        let mut builder = LLMBuilder::new()
            .provider(request.provider_name)
            .model(request.model);
        if let Some(params) = request.params.as_ref() {
            builder = builder.parameters_from_value(params);
        }
        let provider = builder
            .build_with(&self.registry)
            .await
            .map_err(|e| RemoteProviderHostError::Internal(e.to_string()))?;
        Ok(Arc::from(provider))
    }
}

pub struct ModelAllowlistBackend<T> {
    inner: T,
    allowed: BTreeMap<String, Option<BTreeSet<String>>>,
}

impl<T> ModelAllowlistBackend<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            allowed: BTreeMap::new(),
        }
    }

    pub fn allow_provider(mut self, provider: impl Into<String>) -> Self {
        self.allowed.insert(provider.into(), None);
        self
    }

    pub fn allow_models(
        mut self,
        provider: impl Into<String>,
        models: impl IntoIterator<Item = String>,
    ) -> Self {
        self.allowed
            .insert(provider.into(), Some(models.into_iter().collect()));
        self
    }

    fn allows(&self, provider: &str, model: &str) -> bool {
        match self.allowed.get(provider) {
            Some(None) => true,
            Some(Some(models)) => models.contains(model),
            None => false,
        }
    }
}

#[async_trait]
impl<T> RemoteProviderBackend for ModelAllowlistBackend<T>
where
    T: RemoteProviderBackend<Error = RemoteProviderHostError> + Send + Sync,
{
    type Error = RemoteProviderHostError;

    async fn build_provider(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
        if !self.allows(&request.provider_name, &request.model) {
            return Err(RemoteProviderHostError::Internal(format!(
                "provider/model not shared: provider='{}' model='{}'",
                request.provider_name, request.model
            )));
        }
        self.inner.build_provider(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{ChatMessage, ChatProvider, ChatResponse, Tool};
    use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
    use querymt::embedding::EmbeddingProvider;
    use querymt::error::LLMError;
    use std::sync::Mutex;

    struct DummyProvider;

    #[async_trait]
    impl ChatProvider for DummyProvider {
        fn supports_streaming(&self) -> bool {
            false
        }

        async fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<Box<dyn ChatResponse>, LLMError> {
            Err(LLMError::NotImplemented("dummy".into()))
        }

        async fn chat_stream_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures_util::Stream<Item = Result<querymt::chat::StreamChunk, LLMError>>
                        + Send,
                >,
            >,
            LLMError,
        > {
            Err(LLMError::NotImplemented("dummy".into()))
        }
    }

    #[async_trait]
    impl CompletionProvider for DummyProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("dummy".into()))
        }
    }

    #[async_trait]
    impl EmbeddingProvider for DummyProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("dummy".into()))
        }
    }

    impl LLMProvider for DummyProvider {}

    struct RecordingBackend {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl RemoteProviderBackend for RecordingBackend {
        type Error = RemoteProviderHostError;

        async fn build_provider(
            &self,
            request: ProviderBuildRequest,
        ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
            self.calls
                .lock()
                .unwrap()
                .push((request.provider_name, request.model));
            Ok(Arc::new(DummyProvider))
        }
    }

    #[test]
    fn static_catalog_backend_builders_populate_snapshot() {
        let single = StaticCatalogBackend::provider_model(
            "node-1",
            Some("Node One".to_string()),
            "openai",
            "gpt-4o",
        )
        .snapshot();
        assert_eq!(single.node.node_id, "node-1");
        assert_eq!(single.node.node_label.as_deref(), Some("Node One"));
        assert_eq!(single.providers.len(), 1);
        assert_eq!(single.providers[0].provider, "openai");
        assert_eq!(single.providers[0].model.as_deref(), Some("gpt-4o"));

        let multiple = StaticCatalogBackend::providers(
            "node-2",
            None,
            ["openai".to_string(), "anthropic".to_string()],
        )
        .snapshot();
        assert_eq!(multiple.providers.len(), 2);
        assert!(
            multiple
                .providers
                .iter()
                .any(|entry| entry.provider == "openai")
        );
        assert!(
            multiple
                .providers
                .iter()
                .any(|entry| entry.provider == "anthropic")
        );
    }

    #[tokio::test]
    async fn static_provider_backend_returns_same_provider_arc() {
        let provider: Arc<dyn LLMProvider> = Arc::new(DummyProvider);
        let backend = StaticProviderBackend::new(Arc::clone(&provider));
        let built = backend
            .build_provider(ProviderBuildRequest::new("openai", "gpt-4o"))
            .await
            .unwrap();

        assert!(Arc::ptr_eq(&provider, &built));
    }

    #[tokio::test]
    async fn closure_backend_invokes_builder_with_request() {
        let backend = ClosureProviderBackend::new(|request: ProviderBuildRequest| async move {
            assert_eq!(request.provider_name, "openai");
            assert_eq!(request.model, "gpt-4o");
            Ok::<Arc<dyn LLMProvider>, RemoteProviderHostError>(Arc::new(DummyProvider))
        });

        backend
            .build_provider(ProviderBuildRequest::new("openai", "gpt-4o"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn model_allowlist_backend_accepts_allowed_models_and_rejects_others() {
        let inner = RecordingBackend::new();
        let backend = ModelAllowlistBackend::new(inner)
            .allow_provider("openai")
            .allow_models("anthropic", ["claude-3-7".to_string()]);

        backend
            .build_provider(ProviderBuildRequest::new("openai", "gpt-4o"))
            .await
            .unwrap();
        backend
            .build_provider(ProviderBuildRequest::new("anthropic", "claude-3-7"))
            .await
            .unwrap();

        let err = backend
            .build_provider(ProviderBuildRequest::new("anthropic", "claude-3-5"))
            .await
            .err()
            .expect("disallowed model should fail");
        match err {
            RemoteProviderHostError::Internal(message) => {
                assert!(message.contains("provider/model not shared"));
                assert!(message.contains("anthropic"));
                assert!(message.contains("claude-3-5"));
            }
            other => panic!("expected internal allowlist error, got {other:?}"),
        }

        let err = backend
            .build_provider(ProviderBuildRequest::new("google", "gemini-1.5"))
            .await
            .err()
            .expect("unknown provider should fail");
        assert!(matches!(err, RemoteProviderHostError::Internal(_)));

        let calls = backend.inner.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], ("openai".to_string(), "gpt-4o".to_string()));
        assert_eq!(
            calls[1],
            ("anthropic".to_string(), "claude-3-7".to_string())
        );
    }
}
