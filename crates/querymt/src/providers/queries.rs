use super::types::{ModelInfo, ProviderInfo, ProvidersRegistry};

impl ProvidersRegistry {
    pub fn get_provider(&self, id: &str) -> Option<&ProviderInfo> {
        self.providers.get(id)
    }

    pub fn get_model(&self, provider: &str, model: &str) -> Option<&ModelInfo> {
        self.providers
            .get(provider)
            .and_then(|provider| provider.models.get(model))
    }

    pub fn list_providers(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    pub fn list_models(&self, provider: &str) -> Vec<&str> {
        self.providers
            .get(provider)
            .map(|provider| provider.models.keys().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    pub fn get_pricing(&self, provider: &str, model: &str) -> Option<&super::types::ModelPricing> {
        self.get_model(provider, model).map(|m| &m.pricing)
    }

    pub fn get_constraints(
        &self,
        provider: &str,
        model: &str,
    ) -> Option<&super::types::ModelConstraints> {
        self.get_model(provider, model).map(|m| &m.constraints)
    }

    pub fn get_capabilities(
        &self,
        provider: &str,
        model: &str,
    ) -> Option<&super::types::ModelCapabilities> {
        self.get_model(provider, model).map(|m| &m.capabilities)
    }
}

