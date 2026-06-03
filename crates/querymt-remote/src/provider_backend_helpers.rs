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
