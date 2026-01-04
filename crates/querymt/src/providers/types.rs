use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct ProvidersRegistry {
    pub providers: HashMap<String, ProviderInfo>,
}

impl From<HashMap<String, ProviderInfo>> for ProvidersRegistry {
    fn from(map: HashMap<String, ProviderInfo>) -> Self {
        ProvidersRegistry { providers: map }
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderInfo {
    pub id: String,
    #[serde(default)]
    pub env: Vec<String>,
    pub npm: Option<String>,
    pub name: String,
    pub doc: Option<String>,
    #[serde(default)]
    pub models: HashMap<String, ModelInfo>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub capabilities: ModelCapabilities,
    #[serde(rename = "limit", default)]
    pub constraints: ModelConstraints,
    #[serde(rename = "cost", default)]
    pub pricing: ModelPricing,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct ModelCapabilities {
    pub attachment: bool,
    pub reasoning: bool,
    pub temperature: bool,
    pub tool_call: bool,
    pub modalities: Modalities,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Modalities {
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct ModelConstraints {
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    pub context: Option<u64>,
    pub output: Option<u64>,
    pub open_weights: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct ModelPricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl ModelPricing {
    /// Calculate total cost from token usage
    ///
    /// Returns cost in USD, or None if pricing information is incomplete
    pub fn calculate_cost(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        let input_cost = self.input?;
        let output_cost = self.output?;

        let input_total = (input_tokens as f64 / 1_000_000.0) * input_cost;
        let output_total = (output_tokens as f64 / 1_000_000.0) * output_cost;

        Some(input_total + output_total)
    }

    /// Calculate cache costs if available
    ///
    /// Returns (cache_read_cost, cache_write_cost) in USD
    pub fn calculate_cache_cost(
        &self,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) -> (Option<f64>, Option<f64>) {
        let read_cost = self
            .cache_read
            .map(|rate| (cache_read_tokens as f64 / 1_000_000.0) * rate);

        let write_cost = self
            .cache_write
            .map(|rate| (cache_write_tokens as f64 / 1_000_000.0) * rate);

        (read_cost, write_cost)
    }
}

impl ModelCapabilities {
    /// Check if model supports required capabilities
    ///
    /// Returns true only if all required capabilities are supported
    pub fn supports(&self, requires_tools: bool, requires_attachments: bool) -> bool {
        let tools_ok = !requires_tools || self.tool_call;
        let attachments_ok = !requires_attachments || self.attachment;

        tools_ok && attachments_ok
    }

    /// Check if model supports tool calling
    pub fn supports_tools(&self) -> bool {
        self.tool_call
    }

    /// Check if model supports attachments (images, audio, video)
    pub fn supports_attachments(&self) -> bool {
        self.attachment
    }

    /// Check if model supports reasoning/thinking
    pub fn supports_reasoning(&self) -> bool {
        self.reasoning
    }
}

impl ModelInfo {
    /// Calculate cost for this model given token usage
    pub fn calculate_cost(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        self.pricing.calculate_cost(input_tokens, output_tokens)
    }

    /// Check if model supports required capabilities
    pub fn supports(&self, requires_tools: bool, requires_attachments: bool) -> bool {
        self.capabilities
            .supports(requires_tools, requires_attachments)
    }

    /// Get context window limit in tokens
    pub fn context_limit(&self) -> Option<u64> {
        self.constraints.context
    }

    /// Get maximum output tokens
    pub fn output_limit(&self) -> Option<u64> {
        self.constraints.output
    }

    /// Check if requested output tokens are within model's limit
    pub fn validate_output_limit(&self, requested_tokens: u64) -> Result<(), String> {
        if let Some(limit) = self.constraints.output {
            if requested_tokens > limit {
                return Err(format!(
                    "Requested {} output tokens exceeds model limit of {}",
                    requested_tokens, limit
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pricing_calculate_cost() {
        let pricing = ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: None,
            cache_write: None,
        };

        let cost = pricing.calculate_cost(1_000_000, 500_000);
        assert_eq!(cost, Some(10.5)); // (1M * 3.0 / 1M) + (500k * 15.0 / 1M) = 3 + 7.5
    }

    #[test]
    fn test_pricing_calculate_cost_incomplete() {
        let pricing = ModelPricing {
            input: None,
            output: Some(15.0),
            cache_read: None,
            cache_write: None,
        };

        let cost = pricing.calculate_cost(1_000_000, 500_000);
        assert_eq!(cost, None);
    }

    #[test]
    fn test_pricing_calculate_cache_cost() {
        let pricing = ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: Some(0.3),
            cache_write: Some(3.75),
        };

        let (read, write) = pricing.calculate_cache_cost(1_000_000, 500_000);
        assert_eq!(read, Some(0.3)); // 1M * 0.3 / 1M
        assert_eq!(write, Some(1.875)); // 500k * 3.75 / 1M
    }

    #[test]
    fn test_capabilities_supports() {
        let caps = ModelCapabilities {
            attachment: true,
            reasoning: false,
            temperature: true,
            tool_call: true,
            modalities: Modalities::default(),
        };

        assert!(caps.supports(true, true)); // Both supported
        assert!(caps.supports(true, false)); // Only tools required
        assert!(caps.supports(false, true)); // Only attachments required
        assert!(caps.supports(false, false)); // Nothing required
    }

    #[test]
    fn test_capabilities_supports_missing() {
        let caps = ModelCapabilities {
            attachment: false,
            reasoning: false,
            temperature: true,
            tool_call: true,
            modalities: Modalities::default(),
        };

        assert!(!caps.supports(true, true)); // Attachments not supported
        assert!(caps.supports(true, false)); // Tools supported
        assert!(!caps.supports(false, true)); // Attachments not supported
    }

    #[test]
    fn test_model_info_validate_output_limit() {
        let model = ModelInfo {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            capabilities: ModelCapabilities::default(),
            constraints: ModelConstraints {
                output: Some(4096),
                ..Default::default()
            },
            pricing: ModelPricing::default(),
        };

        assert!(model.validate_output_limit(4000).is_ok());
        assert!(model.validate_output_limit(4096).is_ok());
        assert!(model.validate_output_limit(5000).is_err());
    }

    #[test]
    fn test_model_info_validate_output_limit_no_limit() {
        let model = ModelInfo {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            capabilities: ModelCapabilities::default(),
            constraints: ModelConstraints {
                output: None,
                ..Default::default()
            },
            pricing: ModelPricing::default(),
        };

        // Should pass with any value when no limit is set
        assert!(model.validate_output_limit(999999).is_ok());
    }
}
