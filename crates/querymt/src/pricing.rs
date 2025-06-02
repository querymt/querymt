use dirs;
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_FILE: &str = "openrouter_models.json";
const CACHE_DURATION: u64 = 86_400; // 24 hours in seconds
const API_URL: &str = "https://openrouter.ai/api/v1/models";

fn deserialize_f64_from_string<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    s.parse::<f64>().map_err(serde::de::Error::custom)
}

/// Pricing information for a model from OpenRouter's API.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Pricing {
    #[serde(deserialize_with = "deserialize_f64_from_string", default)]
    pub prompt: f64,
    #[serde(deserialize_with = "deserialize_f64_from_string", default)]
    pub completion: f64,
    #[serde(deserialize_with = "deserialize_f64_from_string", default)]
    pub request: f64,
    #[serde(deserialize_with = "deserialize_f64_from_string", default)]
    pub image: f64,
    #[serde(deserialize_with = "deserialize_f64_from_string", default)]
    pub web_search: f64,
    #[serde(deserialize_with = "deserialize_f64_from_string", default)]
    pub internal_reasoning: f64,
}

/// Model data with ID and pricing from OpenRouter's API.
#[derive(Debug, Serialize, Deserialize)]
struct ModelData {
    id: String,
    pricing: Pricing,
}

/// Response structure for the OpenRouter /models API.
#[derive(Debug, Serialize, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelData>,
}

/// Checks if the cache file exists and is fresh (less than 24 hours old).
fn is_cache_fresh() -> bool {
    if let Ok(metadata) = fs::metadata(CACHE_FILE) {
        if let Ok(modified) = metadata.modified() {
            if let Ok(modified_time) = modified.duration_since(UNIX_EPOCH) {
                if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
                    return now.as_secs() - modified_time.as_secs() < CACHE_DURATION;
                }
            }
        }
    }
    false
}

/// Loads cached JSON from file or fetches from OpenRouter API if stale.
async fn load_or_fetch_models() -> Result<Vec<ModelData>, Box<dyn std::error::Error>> {
    let home_dir = dirs::home_dir().expect("Could not find home directory");
    let file_path = home_dir.join(".qmt").join(CACHE_FILE);

    if is_cache_fresh() {
        let mut file = File::open(file_path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let response: ModelsResponse = serde_json::from_str(&contents)?;
        return Ok(response.data);
    }

    let client = Client::new();
    let response = client
        .get(API_URL)
        .send()
        .await?
        .json::<ModelsResponse>()
        .await?;

    let json = serde_json::to_string(&response)?;
    let mut file = File::create(file_path)?;
    file.write_all(json.as_bytes())?;

    Ok(response.data)
}

pub async fn get_model_pricing(model_name: &str) -> Option<Pricing> {
    match load_or_fetch_models().await {
        Ok(models) => models
            .iter()
            .find(|m| m.id == model_name)
            .map(|m| m.pricing.clone()),
        Err(e) => {
            // Debug: Print error if API call fails
            log::error!("API error: {:?}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[tokio::test]
    async fn test_get_model_pricing_known_models() {
        use std::fs;
        use std::path::Path;

        let cache_dir = Path::new("/tmp/.qmt/");
        fs::create_dir_all(cache_dir).unwrap();
        std::env::set_var("HOME", "/tmp/");

        // Remove cache file to force real API call
        let cache_file = cache_dir.join(CACHE_FILE);
        if cache_file.exists() {
            fs::remove_file(&cache_file).unwrap();
        }

        // All unique model names from your list
        static MODEL_NAMES: &[&str] = &[
            // OpenAI Codex
            "openai/codex-mini",
            // OpenAI GPT o4
            "openai/o4-mini-high",
            "openai/o4-mini",
            // OpenAI GPT o3
            "openai/o3",
            "openai/o3-mini-high",
            "openai/o3-mini",
            // OpenAI GPT 4.1
            "openai/gpt-4.1",
            "openai/gpt-4.1-mini",
            "openai/gpt-4.1-nano",
            // OpenAI GPT o1
            "openai/o1-pro",
            "openai/o1",
            "openai/o1-preview",
            "openai/o1-preview-2024-09-12",
            "openai/o1-mini",
            "openai/o1-mini-2024-09-12",
            // OpenAI GPT 4o
            "openai/gpt-4o-mini-search-preview",
            "openai/gpt-4o-search-preview",
            "openai/gpt-4o-2024-11-20",
            "openai/chatgpt-4o-latest",
            "openai/gpt-4o-2024-08-06",
            "openai/gpt-4o-mini",
            "openai/gpt-4o-mini-2024-07-18",
            "openai/gpt-4o",
            "openai/gpt-4o:extended",
            "openai/gpt-4o-2024-05-13",
            // OpenAI GPT 4.5
            "openai/gpt-4.5-preview",
            // OpenAI GPT 4
            "openai/gpt-4-turbo",
            "openai/gpt-4-turbo-preview",
            "openai/gpt-4-1106-preview",
            "openai/gpt-4-32k",
            "openai/gpt-4-32k-0314",
            "openai/gpt-4",
            "openai/gpt-4-0314",
            // OpenAI GPT 3.5
            "openai/gpt-3.5-turbo-0613",
            "openai/gpt-3.5-turbo-1106",
            "openai/gpt-3.5-turbo-instruct",
            "openai/gpt-3.5-turbo-16k",
            "openai/gpt-3.5-turbo-0125",
            "openai/gpt-3.5-turbo",
            //
            // Google Gemma 2
            "google/gemma-2b-it",
            "google/gemma-2-9b-it",
            "google/gemma-2-27b-it",
            // Google Gemma 3
            "google/gemma-3-4b-it",
            "google/gemma-3-12b-it",
            "google/gemma-3-27b-it",
            // Google Gemini 2.0
            "google/gemini-2.0-flash-001",
            "google/gemini-2.0-flash-lite-001",
            // Google Gemini 1.5
            "google/gemini-flash-1.5",
            "google/gemini-flash-1.5-8b",
            "google/gemini-pro-1.5",
            // Google Gemini 2.5
            "google/gemini-2.5-pro-preview",
            "google/gemini-2.5-flash-preview",
            "google/gemini-2.5-flash-preview:thinking",
            "google/gemini-2.5-flash-preview-05-20",
            "google/gemini-2.5-flash-preview-05-20:thinking",
            //
            // Anthropic Claude 4
            "anthropic/claude-opus-4",
            "anthropic/claude-sonnet-4",
            // Anthropic Claude 3.7
            "anthropic/claude-3.7-sonnet",
            "anthropic/claude-3.7-sonnet:beta",
            "anthropic/claude-3.7-sonnet:thinking",
            // Anthropic Claude 3.5 - Haiku
            "anthropic/claude-3.5-haiku",
            "anthropic/claude-3.5-haiku:beta",
            "anthropic/claude-3.5-haiku-20241022",
            "anthropic/claude-3.5-haiku-20241022:beta",
            // Anthropic Claude 3.5 - Sonnet
            "anthropic/claude-3.5-sonnet",
            "anthropic/claude-3.5-sonnet:beta",
            "anthropic/claude-3.5-sonnet-20240620",
            "anthropic/claude-3.5-sonnet-20240620:beta",
            // Anthropic Claude 3 - Haiku
            "anthropic/claude-3-haiku",
            "anthropic/claude-3-haiku:beta",
            // Anthropic Claude 3 - Opus
            "anthropic/claude-3-opus",
            "anthropic/claude-3-opus:beta",
            // Anthropic Claude 3 - Sonnet
            "anthropic/claude-3-sonnet",
            "anthropic/claude-3-sonnet:beta",
            // Anthropic Claude 2.1
            "anthropic/claude-2.1",
            "anthropic/claude-2.1:beta",
            // Anthropic Claude 2.0
            "anthropic/claude-2.0",
            "anthropic/claude-2.0:beta",
            // Anthropic Claude 2
            "anthropic/claude-2",
            "anthropic/claude-2:beta",
            //
            // MistralAI Devstral
            "mistralai/devstral-small",
            // MistralAI Codestral
            "mistralai/codestral-2501",
            // MistralAI Pixtral
            "mistralai/pixtral-large-2411",
            "mistralai/pixtral-12b",
            // MistralAI Ministral
            "mistralai/ministral-8b",
            "mistralai/ministral-3b",
            // MistralAI Mistral Large
            "mistralai/mistral-large",
            "mistralai/mistral-large-2411",
            "mistralai/mistral-large-2407",
            // MistralAI Mistral Medium
            "mistralai/mistral-medium",
            "mistralai/mistral-medium-3",
            // MistralAI Mistral Small
            "mistralai/mistral-small",
            "mistralai/mistral-small-3.1-24b-instruct",
            "mistralai/mistral-small-24b-instruct-2501",
            // MistralAI Mistral Saba
            "mistralai/mistral-saba",
            // MistralAI Mistral Nemo
            "mistralai/mistral-nemo",
            // MistralAI Mistral Tiny
            "mistralai/mistral-tiny",
            // MistralAI Mistral 7B Instruct
            "mistralai/mistral-7b-instruct",
            "mistralai/mistral-7b-instruct-v0.3",
            "mistralai/mistral-7b-instruct-v0.2",
            "mistralai/mistral-7b-instruct-v0.1",
            // MistralAI Mixtral
            "mistralai/mixtral-8x22b-instruct",
            "mistralai/mixtral-8x7b-instruct",
            //
            // DeepSeek R1 Distill
            "deepseek/deepseek-r1-distill-qwen-7b",
            "deepseek/deepseek-r1-distill-qwen-1.5b",
            "deepseek/deepseek-r1-distill-qwen-14b",
            "deepseek/deepseek-r1-distill-qwen-32b",
            "deepseek/deepseek-r1-distill-llama-8b",
            "deepseek/deepseek-r1-distill-llama-70b",
            // DeepSeek R1 Qwen3
            "deepseek/deepseek-r1-0528-qwen3-8b",
            // DeepSeek R1 0528
            "deepseek/deepseek-r1-0528",
            // DeepSeek R1
            "deepseek/deepseek-r1",
            // DeepSeek Prover
            "deepseek/deepseek-prover-v2",
            // DeepSeek Chat V3
            "deepseek/deepseek-chat-v3-0324",
            // DeepSeek Chat
            "deepseek/deepseek-chat",
            // You can add more here as needed
        ];

        for model in MODEL_NAMES {
            let pricing = get_model_pricing(model).await;
            assert!(
                pricing.is_some(),
                "Model '{}' not found in API response",
                model
            );

            let pricing = pricing.unwrap();
            assert!(
                pricing.prompt > 0.0,
                "Expected non-zero prompt price for '{}'",
                model
            );
            assert!(
                pricing.completion > 0.0,
                "Expected non-zero completion price for '{}'",
                model
            );
            dbg!(model, &pricing);
        }
    }

    #[tokio::test]
    async fn test_get_model_pricing_unknown_model() {
        let cache_dir = Path::new("/tmp/.qmt/");
        fs::create_dir_all(cache_dir).unwrap();
        std::env::set_var("HOME", "/tmp/");

        // Remove cache file to force real API call
        let cache_file = cache_dir.join(CACHE_FILE);
        if cache_file.exists() {
            fs::remove_file(&cache_file).unwrap();
        }

        // Test unknown model
        let pricing = get_model_pricing("unknown/model").await;
        //println!("Pricing for unknown/model: {:?}", pricing);
        assert!(pricing.is_none(), "Expected None for unknown model");
    }
}
