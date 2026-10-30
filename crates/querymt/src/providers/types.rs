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
    /// Whether this provider supports user-managed custom models.
    #[serde(default)]
    pub supports_custom_models: bool,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub capabilities: ModelCapabilities,
    #[serde(rename = "limit", default)]
    pub limits: ModelLimits,
    #[serde(rename = "cost", default)]
    pub pricing: ModelPricing,
    // Top-level metadata fields from the API
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    pub open_weights: Option<bool>,
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
pub struct ModelLimits {
    pub context: Option<u64>,
    pub output: Option<u64>,
}

/// Kept as a type alias for backward compatibility.
pub type ModelConstraints = ModelLimits;

/// Where a pricing basis came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingSource {
    /// Ingested from the models.dev registry.
    Catalog,
    /// Supplied by user configuration.
    Manual,
}

/// When a recognized pricing dimension changes the price of an attempt.
///
/// Applicability decides whether a dimension this crate cannot value makes a
/// valuation incomplete: only dimensions the reported usage can trigger do.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DimensionApplicability {
    /// Applies when prompt tokens (uncached input + cache reads + cache
    /// writes) exceed `tokens`.
    ContextOver { tokens: u64 },
    /// Applies when the attempt reports reasoning tokens.
    ReasoningTokens,
    /// Applies to a non-text modality that canonical usage cannot observe, so
    /// it is preserved but never makes an estimate incomplete on its own.
    Modality,
    /// Unrecognized shape; assumed to apply (conservative).
    Unspecified,
}

/// A recognized pricing modifier that valuation does not apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricingDimension {
    pub name: String,
    pub applies: DimensionApplicability,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(from = "RawPricing")]
pub struct ModelPricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Base cache-write rate (USD per million tokens), as published by the
    /// catalog. Prices five-minute writes and writes without a duration
    /// breakdown (for Anthropic this is the five-minute price).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Explicit one-hour cache-write rate. models.dev has no such field, so
    /// this comes only from manual configuration; it is never derived from
    /// `cache_write` by a multiplier. Nonzero one-hour writes without it are
    /// reported as incomplete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    /// Known pricing modifiers or non-token charges that valuation cannot
    /// apply. Any entry prevents a complete total.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported_dimensions: Vec<String>,
    /// Recognized modifiers preserved from the registry, with applicability.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<PricingDimension>,
    /// Origin of this pricing basis, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PricingSource>,
}

/// Wire shape of `cost`: our own cache format plus the extra dimensions the
/// models.dev API carries. Lenient on dimension payloads so a new upstream
/// shape degrades to a conservative dimension instead of a load failure.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPricing {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    cache_write_1h: Option<f64>,
    unsupported_dimensions: Vec<String>,
    dimensions: Vec<PricingDimension>,
    source: Option<PricingSource>,
    reasoning: Option<f64>,
    input_audio: Option<f64>,
    output_audio: Option<f64>,
    context_over_200k: Option<serde_json::Value>,
    tiers: Vec<serde_json::Value>,
}

fn rate(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

/// True when every rate the tier states equals the base rate, i.e. it changes
/// nothing and need not be disclosed as a modifier.
fn tier_matches_base(tier: &serde_json::Value, base: &RawPricing) -> bool {
    [
        ("input", base.input),
        ("output", base.output),
        ("cache_read", base.cache_read),
        ("cache_write", base.cache_write),
    ]
    .into_iter()
    .all(|(key, base_rate)| rate(tier, key).is_none_or(|r| Some(r) == base_rate))
}

fn push_context_dimension(out: &mut Vec<PricingDimension>, name: String, tokens: u64) {
    let applies = DimensionApplicability::ContextOver { tokens };
    if !out.iter().any(|d| d.applies == applies) {
        out.push(PricingDimension { name, applies });
    }
}

impl From<RawPricing> for ModelPricing {
    fn from(raw: RawPricing) -> Self {
        let mut dimensions = raw.dimensions.clone();

        if let Some(over) = &raw.context_over_200k
            && !tier_matches_base(over, &raw)
        {
            push_context_dimension(&mut dimensions, "context_over_200k".into(), 200_000);
        }
        for tier in &raw.tiers {
            if tier_matches_base(tier, &raw) {
                continue;
            }
            let spec = tier.get("tier");
            let context_size = spec
                .filter(|s| s.get("type").and_then(|t| t.as_str()) == Some("context"))
                .and_then(|s| s.get("size"))
                .and_then(serde_json::Value::as_u64);
            match context_size {
                Some(tokens) => push_context_dimension(
                    &mut dimensions,
                    format!("context_over_{tokens}"),
                    tokens,
                ),
                None => dimensions.push(PricingDimension {
                    name: "tiers".into(),
                    applies: DimensionApplicability::Unspecified,
                }),
            }
        }
        if let Some(reasoning) = raw.reasoning
            && Some(reasoning) != raw.output
        {
            dimensions.push(PricingDimension {
                name: "reasoning_rate".into(),
                applies: DimensionApplicability::ReasoningTokens,
            });
        }
        for (name, audio) in [
            ("input_audio", raw.input_audio),
            ("output_audio", raw.output_audio),
        ] {
            if audio.is_some() {
                dimensions.push(PricingDimension {
                    name: name.into(),
                    applies: DimensionApplicability::Modality,
                });
            }
        }

        Self {
            input: raw.input,
            output: raw.output,
            cache_read: raw.cache_read,
            cache_write: raw.cache_write,
            cache_write_1h: raw.cache_write_1h,
            unsupported_dimensions: raw.unsupported_dimensions,
            dimensions,
            source: raw.source,
        }
    }
}

impl ModelPricing {
    /// Names of recognized dimensions the given prompt/reasoning usage can
    /// trigger but valuation cannot apply.
    pub fn triggered_dimensions(&self, prompt_tokens: u64, reasoning_tokens: u64) -> Vec<&str> {
        self.dimensions
            .iter()
            .filter(|d| match d.applies {
                DimensionApplicability::ContextOver { tokens } => prompt_tokens > tokens,
                DimensionApplicability::ReasoningTokens => reasoning_tokens > 0,
                DimensionApplicability::Modality => false,
                DimensionApplicability::Unspecified => true,
            })
            .map(|d| d.name.as_str())
            .collect()
    }

    /// True when input and output rates are present and every present rate is
    /// explicitly zero (a priced-at-zero model, not an unknown price).
    pub fn is_explicit_zero(&self) -> bool {
        let is_zero = |r: Option<f64>| r.is_none_or(|v| v == 0.0);
        self.input == Some(0.0)
            && self.output == Some(0.0)
            && is_zero(self.cache_read)
            && is_zero(self.cache_write)
            && is_zero(self.cache_write_1h)
    }

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
        self.limits.context
    }

    /// Get maximum output tokens
    pub fn output_limit(&self) -> Option<u64> {
        self.limits.output
    }

    /// Check if requested output tokens are within model's limit
    pub fn validate_output_limit(&self, requested_tokens: u64) -> Result<(), String> {
        if let Some(limit) = self.limits.output
            && requested_tokens > limit
        {
            return Err(format!(
                "Requested {} output tokens exceeds model limit of {}",
                requested_tokens, limit
            ));
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
            ..Default::default()
        };

        let cost = pricing.calculate_cost(1_000_000, 500_000);
        assert_eq!(cost, Some(10.5)); // (1M * 3.0 / 1M) + (500k * 15.0 / 1M) = 3 + 7.5
    }

    #[test]
    fn test_pricing_calculate_cost_incomplete() {
        let pricing = ModelPricing {
            input: None,
            output: Some(15.0),
            ..Default::default()
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
            ..Default::default()
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
            limits: ModelLimits {
                output: Some(4096),
                ..Default::default()
            },
            ..Default::default()
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
            limits: ModelLimits {
                output: None,
                ..Default::default()
            },
            ..Default::default()
        };

        // Should pass with any value when no limit is set
        assert!(model.validate_output_limit(999999).is_ok());
    }

    /// Recorded models.dev `cost` shapes (see the upstream catalog): a model
    /// with a context tier, one with a distinct reasoning rate and audio, and
    /// an Anthropic-style model with only an aggregate write rate.
    const REGISTRY_FIXTURE: &str = r#"{
        "p": { "id": "p", "name": "P", "models": {
            "tiered": { "id": "tiered", "name": "tiered", "cost": {
                "input": 1.0, "output": 4.0, "cache_read": 0.1,
                "tiers": [{ "input": 2.0, "output": 8.0, "tier": { "type": "context", "size": 200000 } }],
                "context_over_200k": { "input": 2.0, "output": 8.0 } } },
            "reasoner": { "id": "reasoner", "name": "reasoner", "cost": {
                "input": 0.66, "output": 1.98, "reasoning": 3.0, "input_audio": 0.7 } },
            "flat_tier": { "id": "flat_tier", "name": "flat_tier", "cost": {
                "input": 0.09, "output": 0.71,
                "tiers": [{ "input": 0.09, "output": 0.71, "tier": { "type": "context", "size": 200000 } }] } },
            "anthropic": { "id": "anthropic", "name": "anthropic", "cost": {
                "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75 } }
        } }
    }"#;

    fn fixture_pricing(model: &str) -> ModelPricing {
        let map: HashMap<String, ProviderInfo> = serde_json::from_str(REGISTRY_FIXTURE).unwrap();
        map["p"].models[model].pricing.clone()
    }

    #[test]
    fn registry_ingestion_preserves_dimensions_and_round_trips() {
        let tiered = fixture_pricing("tiered");
        assert_eq!(tiered.dimensions.len(), 1, "tier and alias dedupe to one");
        assert_eq!(
            tiered.dimensions[0].applies,
            DimensionApplicability::ContextOver { tokens: 200_000 }
        );
        let reasoner = fixture_pricing("reasoner");
        let names: Vec<_> = reasoner
            .dimensions
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, ["reasoning_rate", "input_audio"]);
        // A tier that restates the base rates changes nothing.
        assert!(fixture_pricing("flat_tier").dimensions.is_empty());

        // Cache serialization keeps every recognized dimension and metadata.
        let mut stamped = tiered.clone();
        stamped.source = Some(PricingSource::Catalog);
        let reloaded: ModelPricing =
            serde_json::from_str(&serde_json::to_string(&stamped).unwrap()).unwrap();
        assert_eq!(reloaded.dimensions, stamped.dimensions);
        assert_eq!(reloaded.source, Some(PricingSource::Catalog));
    }

    #[test]
    fn catalog_write_rate_is_base_and_one_hour_rate_stays_missing() {
        let anthropic = fixture_pricing("anthropic");
        assert_eq!(anthropic.cache_write, Some(3.75));
        // models.dev has no one-hour rate; none is synthesized.
        assert_eq!(anthropic.cache_write_1h, None);
        let json = serde_json::to_string(&anthropic).unwrap();
        assert!(!json.contains("cache_write_1h"));
        let reloaded: ModelPricing = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded.cache_write, Some(3.75));
        assert_eq!(reloaded.cache_write_1h, None);

        // An explicit (manual) one-hour rate round-trips.
        let mut manual = anthropic.clone();
        manual.cache_write_1h = Some(6.0);
        let reloaded: ModelPricing =
            serde_json::from_str(&serde_json::to_string(&manual).unwrap()).unwrap();
        assert_eq!(reloaded.cache_write_1h, Some(6.0));
    }

    #[test]
    fn triggered_dimensions_depend_on_usage() {
        let tiered = fixture_pricing("tiered");
        assert!(tiered.triggered_dimensions(200_000, 0).is_empty());
        assert_eq!(
            tiered.triggered_dimensions(200_001, 0),
            ["context_over_200k"]
        );
        let reasoner = fixture_pricing("reasoner");
        assert!(
            reasoner.triggered_dimensions(10, 0).is_empty(),
            "audio never triggers"
        );
        assert_eq!(reasoner.triggered_dimensions(10, 5), ["reasoning_rate"]);
    }

    #[test]
    fn test_deserialize_flat_api_format() {
        let json = r#"{
            "id": "mistral-nemo",
            "name": "Mistral Nemo",
            "attachment": false,
            "reasoning": false,
            "tool_call": true,
            "temperature": true,
            "knowledge": "2024-07",
            "release_date": "2024-07-01",
            "last_updated": "2024-07-01",
            "modalities": { "input": ["text"], "output": ["text"] },
            "open_weights": true,
            "cost": { "input": 0.15, "output": 0.15 },
            "limit": { "context": 128000, "output": 128000 }
        }"#;
        let model: ModelInfo = serde_json::from_str(json).unwrap();

        assert_eq!(model.id, "mistral-nemo");
        assert!(model.capabilities.tool_call);
        assert!(model.capabilities.temperature);
        assert!(!model.capabilities.attachment);
        assert!(!model.capabilities.reasoning);
        assert_eq!(model.capabilities.modalities.input, vec!["text"]);
        assert_eq!(model.capabilities.modalities.output, vec!["text"]);
        assert_eq!(model.knowledge.as_deref(), Some("2024-07"));
        assert_eq!(model.release_date.as_deref(), Some("2024-07-01"));
        assert_eq!(model.open_weights, Some(true));
        assert_eq!(model.limits.context, Some(128000));
        assert_eq!(model.limits.output, Some(128000));
        assert_eq!(model.pricing.input, Some(0.15));
        assert_eq!(model.pricing.output, Some(0.15));
    }
}
