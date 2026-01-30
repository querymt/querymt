use super::types::{ModelInfo, ProviderInfo, ProvidersRegistry};

impl ProvidersRegistry {
    pub fn get_provider(&self, id: &str) -> Option<&ProviderInfo> {
        self.providers.get(id)
    }

    pub fn get_model(&self, provider: &str, model: &str) -> Option<&ModelInfo> {
        let result = self
            .providers
            .get(provider)
            .and_then(|provider| provider.models.get(model));

        // Fallback: if provider is "codex" and model not found, try "openai"
        if result.is_none() && provider == "codex" {
            return self
                .providers
                .get("openai")
                .and_then(|provider| provider.models.get(model));
        }

        result
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn create_test_registry() -> ProvidersRegistry {
        let mut providers = HashMap::new();

        // Create openai provider with a test model
        let mut openai_models = HashMap::new();
        openai_models.insert(
            "gpt-4".to_string(),
            ModelInfo {
                id: "gpt-4".to_string(),
                name: "GPT-4".to_string(),
                ..Default::default()
            },
        );

        providers.insert(
            "openai".to_string(),
            ProviderInfo {
                id: "openai".to_string(),
                name: "OpenAI".to_string(),
                models: openai_models,
                ..Default::default()
            },
        );

        // Create codex provider without the model
        providers.insert(
            "codex".to_string(),
            ProviderInfo {
                id: "codex".to_string(),
                name: "Codex".to_string(),
                models: HashMap::new(),
                ..Default::default()
            },
        );

        ProvidersRegistry { providers }
    }

    #[test]
    fn test_get_model_direct_lookup() {
        let registry = create_test_registry();

        // Direct lookup should work
        assert!(registry.get_model("openai", "gpt-4").is_some());
    }

    #[test]
    fn test_get_model_codex_fallback() {
        let registry = create_test_registry();

        // Codex provider exists but model doesn't, should fallback to openai
        let model = registry.get_model("codex", "gpt-4");
        assert!(model.is_some());
        assert_eq!(model.unwrap().id, "gpt-4");
    }

    #[test]
    fn test_get_model_codex_no_fallback_when_found() {
        let mut registry = create_test_registry();

        // Add a model to codex
        let codex_provider = registry.providers.get_mut("codex").unwrap();
        codex_provider.models.insert(
            "codex-model".to_string(),
            ModelInfo {
                id: "codex-model".to_string(),
                name: "Codex Model".to_string(),
                ..Default::default()
            },
        );

        // Should find in codex, not fallback
        let model = registry.get_model("codex", "codex-model");
        assert!(model.is_some());
        assert_eq!(model.unwrap().id, "codex-model");
    }

    #[test]
    fn test_get_model_non_codex_no_fallback() {
        let registry = create_test_registry();

        // Non-codex provider shouldn't fallback
        assert!(registry.get_model("openai", "nonexistent").is_none());
        assert!(registry.get_model("other", "gpt-4").is_none());
    }

    #[test]
    fn test_fallback_propagates_through_helper_methods() {
        let registry = create_test_registry();

        // Pricing, constraints, and capabilities should also work through fallback
        assert!(registry.get_pricing("codex", "gpt-4").is_some());
        assert!(registry.get_constraints("codex", "gpt-4").is_some());
        assert!(registry.get_capabilities("codex", "gpt-4").is_some());
    }
}
