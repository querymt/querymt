use serde::{Deserialize, Deserializer, Serialize};

fn pricing_deserializer<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrFloat {
        String(String),
        Float(f64),
    }

    match StringOrFloat::deserialize(deserializer)? {
        StringOrFloat::String(s) => s.parse().map_err(serde::de::Error::custom),
        StringOrFloat::Float(f) => Ok(f),
    }
}

/// Pricing information for a model from OpenRouter's API.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Pricing {
    #[serde(deserialize_with = "pricing_deserializer", default)]
    pub prompt: f64,
    #[serde(deserialize_with = "pricing_deserializer", default)]
    pub completion: f64,
    #[serde(deserialize_with = "pricing_deserializer", default)]
    pub request: f64,
    #[serde(deserialize_with = "pricing_deserializer", default)]
    pub image: f64,
    #[serde(deserialize_with = "pricing_deserializer", default)]
    pub web_search: f64,
    #[serde(deserialize_with = "pricing_deserializer", default)]
    pub internal_reasoning: f64,
}

/// Model data with ID and pricing from OpenRouter's API.
#[derive(Debug, Serialize, Deserialize)]
pub struct ModelPricing {
    pub id: String,
    pub pricing: Pricing,
}

/// Response structure for the OpenRouter /models API.
#[derive(Debug, Serialize, Deserialize)]
pub struct ModelsPricingData {
    pub data: Vec<ModelPricing>,
}
