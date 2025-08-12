use dirs;
use reqwest::Client;
use std::fmt::Debug;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::ProvidersRegistry;
use crate::error::LLMError;

const CACHE_FILE: &str = "models.dev.json";
const CACHE_DURATION: u64 = 86_400; // 24 hours in seconds
const API_URL: &str = "https://models.dev/api.json";

fn is_cache_fresh(file_path: &Path) -> bool {
    if let Ok(metadata) = fs::metadata(file_path) {
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

async fn download_and_cache_providers(file_path: &Path) -> Result<ProvidersRegistry, LLMError> {
    let client = Client::new();
    let response = client.get(API_URL).send().await?;

    if !response.status().is_success() {
        return Err(LLMError::ProviderError(
            format!("HTTP Error: {}", response.status()).into(),
        ));
    }

    // API returns a top-level map of providers, convert into ProvidersRegistry
    let map = response
        .json::<std::collections::HashMap<String, super::types::ProviderInfo>>()
        .await;

    // TODO: Use LLMError
    let map = match map {
        Ok(r) => r,
        Err(e) => panic!("{}", e),
    };

    let registry: ProvidersRegistry = map.into();

    let json = serde_json::to_string(&registry)?;
    fs::create_dir_all(file_path.parent().unwrap())?;
    let mut file = File::create(file_path)?;
    file.write_all(json.as_bytes())?;

    Ok(registry)
}

pub fn read_providers_from_cache() -> Result<ProvidersRegistry, LLMError> {
    let home_dir = dirs::home_dir().expect("Could not find home directory");
    let file_path = home_dir.join(".qmt").join(CACHE_FILE);

    let mut file = File::open(file_path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let registry: ProvidersRegistry = serde_json::from_str(&contents)?;
    Ok(registry)
}

pub async fn update_providers_if_stale() -> Result<bool, LLMError> {
    let home_dir = dirs::home_dir().expect("Could not find home directory");
    let file_path = home_dir.join(".qmt").join(CACHE_FILE);

    if is_cache_fresh(&file_path) {
        return Ok(false);
    }

    download_and_cache_providers(&file_path).await?;
    Ok(true)
}

