use log::warn;
use querymt::providers::{ModelInfo, ModelPricing, PricingSource, providers_registry};

/// A manual pricing rate that is negative or not finite.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("invalid manual pricing: {field} = {value} (must be finite and nonnegative)")]
pub struct InvalidManualPricing {
    pub field: &'static str,
    pub value: f64,
}

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
        /// Cache-read rate. Never inferred from the input rate.
        cache_read_cost_per_million: Option<f64>,
        /// Base cache-write rate (five-minute or unspecified duration).
        cache_write_cost_per_million: Option<f64>,
        /// Optional explicit one-hour cache-write rate. Never derived from
        /// the base write rate.
        cache_write_1h_cost_per_million: Option<f64>,
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
            cache_read_cost_per_million: None,
            cache_write_cost_per_million: None,
            cache_write_1h_cost_per_million: None,
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

    /// Set explicit cache rates (USD per million tokens) for manual source.
    pub fn cache_pricing(
        mut self,
        read: Option<f64>,
        write: Option<f64>,
        write_1h: Option<f64>,
    ) -> Self {
        if let Self::Manual {
            ref mut cache_read_cost_per_million,
            ref mut cache_write_cost_per_million,
            ref mut cache_write_1h_cost_per_million,
            ..
        } = self
        {
            *cache_read_cost_per_million = read;
            *cache_write_cost_per_million = write;
            *cache_write_1h_cost_per_million = write_1h;
        }
        self
    }

    /// The manual budget pricing, if this is a manual source with any rate.
    ///
    /// Every configured rate must be finite and nonnegative; otherwise the
    /// configuration is invalid. Unset rates stay unset: cache categories are
    /// never priced at the input rate, so a used category without a rate
    /// yields a partial budget valuation.
    pub fn budget_pricing(&self) -> Result<Option<ModelPricing>, InvalidManualPricing> {
        let Self::Manual {
            input_cost_per_million,
            output_cost_per_million,
            cache_read_cost_per_million,
            cache_write_cost_per_million,
            cache_write_1h_cost_per_million,
            ..
        } = self
        else {
            return Ok(None);
        };
        let rates = [
            ("input_cost_per_million", *input_cost_per_million),
            ("output_cost_per_million", *output_cost_per_million),
            ("cache_read_cost_per_million", *cache_read_cost_per_million),
            (
                "cache_write_cost_per_million",
                *cache_write_cost_per_million,
            ),
            (
                "cache_write_1h_cost_per_million",
                *cache_write_1h_cost_per_million,
            ),
        ];
        for (field, rate) in rates {
            if let Some(rate) = rate
                && (!rate.is_finite() || rate < 0.0)
            {
                return Err(InvalidManualPricing { field, value: rate });
            }
        }
        if rates.iter().all(|(_, rate)| rate.is_none()) {
            return Ok(None);
        }
        Ok(Some(ModelPricing {
            input: *input_cost_per_million,
            output: *output_cost_per_million,
            cache_read: *cache_read_cost_per_million,
            cache_write: *cache_write_cost_per_million,
            cache_write_1h: *cache_write_1h_cost_per_million,
            source: Some(PricingSource::Manual),
            ..Default::default()
        }))
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
    match providers_registry() {
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
    fn budget_pricing_carries_cache_rates_and_never_defaults_them() {
        let source =
            ModelInfoSource::manual()
                .pricing(3.0, 15.0)
                .cache_pricing(Some(0.3), None, Some(6.0));
        let p = source.budget_pricing().unwrap().unwrap();
        assert_eq!(p.input, Some(3.0));
        assert_eq!(p.output, Some(15.0));
        assert_eq!(p.cache_read, Some(0.3));
        assert_eq!(p.cache_write, None, "unset write rate is not inferred");
        assert_eq!(p.cache_write_1h, Some(6.0));
        assert_eq!(p.source, Some(PricingSource::Manual));
    }

    #[test]
    fn budget_pricing_is_absent_without_rates_or_for_session_source() {
        assert_eq!(ModelInfoSource::manual().budget_pricing(), Ok(None));
        assert_eq!(ModelInfoSource::FromSession.budget_pricing(), Ok(None));
    }

    #[test]
    fn budget_pricing_accepts_explicit_zero_rates() {
        let p = ModelInfoSource::manual()
            .cache_pricing(Some(0.0), Some(0.0), None)
            .budget_pricing()
            .unwrap()
            .unwrap();
        assert_eq!(p.cache_read, Some(0.0));
    }

    #[test]
    fn budget_pricing_rejects_negative_and_non_finite_rates() {
        for (bad, field) in [
            (
                ModelInfoSource::manual().pricing(-1.0, 1.0),
                "input_cost_per_million",
            ),
            (
                ModelInfoSource::manual().pricing(1.0, f64::NAN),
                "output_cost_per_million",
            ),
            (
                ModelInfoSource::manual().cache_pricing(Some(f64::INFINITY), None, None),
                "cache_read_cost_per_million",
            ),
            (
                ModelInfoSource::manual().cache_pricing(None, Some(-0.5), None),
                "cache_write_cost_per_million",
            ),
            (
                ModelInfoSource::manual().cache_pricing(None, None, Some(f64::NEG_INFINITY)),
                "cache_write_1h_cost_per_million",
            ),
        ] {
            assert_eq!(bad.budget_pricing().unwrap_err().field, field);
        }
    }

    #[test]
    fn test_default_is_from_session() {
        let source = ModelInfoSource::default();
        assert!(matches!(source, ModelInfoSource::FromSession));
    }
}
