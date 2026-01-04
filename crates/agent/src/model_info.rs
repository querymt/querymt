use log::warn;
use querymt::providers::{ModelInfo, read_providers_from_cache};

/// Source for model information (capabilities, constraints, pricing)
///
/// This enum allows middleware to choose between dynamic lookup from the
/// session's current model, or manual configuration with hardcoded values.
#[derive(Debug, Default, Clone)]
pub enum ModelInfoSource {
    /// Manual configuration - use provided values
    Manual {
        context_limit: Option<usize>,
        output_limit: Option<u64>,
        supports_tools: bool,
        supports_attachments: bool,
        supports_reasoning: bool,
        input_cost_per_million: Option<f64>,
        output_cost_per_million: Option<f64>,
    },
    /// Dynamic lookup from session's current model
    /// Fetches fresh model info on each use
    #[default]
    FromSession,
}

impl ModelInfoSource {
    /// Create a manual source with commonly used defaults
    pub fn manual() -> Self {
        Self::Manual {
            context_limit: Some(32_000),
            output_limit: Some(4_096),
            supports_tools: true,
            supports_attachments: false,
            supports_reasoning: false,
            input_cost_per_million: None,
            output_cost_per_million: None,
        }
    }

    /// Set context limit for manual source
    pub fn context_limit(mut self, limit: usize) -> Self {
        if let Self::Manual {
            ref mut context_limit,
            ..
        } = self
        {
            *context_limit = Some(limit);
        }
        self
    }

    /// Set output limit for manual source
    pub fn output_limit(mut self, limit: u64) -> Self {
        if let Self::Manual {
            ref mut output_limit,
            ..
        } = self
        {
            *output_limit = Some(limit);
        }
        self
    }

    /// Set pricing for manual source
    pub fn pricing(mut self, input: f64, output: f64) -> Self {
        if let Self::Manual {
            ref mut input_cost_per_million,
            ref mut output_cost_per_million,
            ..
        } = self
        {
            *input_cost_per_million = Some(input);
            *output_cost_per_million = Some(output);
        }
        self
    }

    /// Set capability flags for manual source
    pub fn capabilities(mut self, tools: bool, attachments: bool, reasoning: bool) -> Self {
        if let Self::Manual {
            ref mut supports_tools,
            ref mut supports_attachments,
            ref mut supports_reasoning,
            ..
        } = self
        {
            *supports_tools = tools;
            *supports_attachments = attachments;
            *supports_reasoning = reasoning;
        }
        self
    }
}

/// Helper function to get model info from registry
///
/// This is a convenience function that handles cache reading and logging.
/// Returns None if registry can't be loaded or model not found.
pub fn get_model_info(provider: &str, model: &str) -> Option<ModelInfo> {
    match read_providers_from_cache() {
        Ok(registry) => registry.get_model(provider, model).cloned(),
        Err(e) => {
            warn!("Failed to load providers registry: {}", e);
            None
        }
    }
}

/// Error type for capability validation failures
#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("Model {provider}/{model} not found in registry")]
    ModelNotFound { provider: String, model: String },

    #[error("Model {provider}/{model} does not support tool calling")]
    ToolCallNotSupported { provider: String, model: String },

    #[error("Model {provider}/{model} does not support attachments")]
    AttachmentNotSupported { provider: String, model: String },

    #[error(
        "Requested {requested} output tokens exceeds model {provider}/{model} limit of {limit}"
    )]
    OutputLimitExceeded {
        provider: String,
        model: String,
        requested: u64,
        limit: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_info_source_builder() {
        let source = ModelInfoSource::manual()
            .context_limit(16_000)
            .output_limit(2_048)
            .pricing(1.5, 7.5)
            .capabilities(true, false, false);

        if let ModelInfoSource::Manual {
            context_limit,
            output_limit,
            input_cost_per_million,
            output_cost_per_million,
            supports_tools,
            supports_attachments,
            ..
        } = source
        {
            assert_eq!(context_limit, Some(16_000));
            assert_eq!(output_limit, Some(2_048));
            assert_eq!(input_cost_per_million, Some(1.5));
            assert_eq!(output_cost_per_million, Some(7.5));
            assert!(supports_tools);
            assert!(!supports_attachments);
        } else {
            panic!("Expected Manual variant");
        }
    }

    #[test]
    fn test_default_is_from_session() {
        let source = ModelInfoSource::default();
        assert!(matches!(source, ModelInfoSource::FromSession));
    }
}
