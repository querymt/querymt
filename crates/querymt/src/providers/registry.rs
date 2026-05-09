use reqwest::Client;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::ProvidersRegistry;
use crate::error::LLMError;

const CACHE_FILE: &str = "models.dev.json";
const CACHE_DURATION: u64 = 86_400; // 24 hours in seconds
const API_URL: &str = "https://models.dev/api.json";

fn provider_cache_dir() -> Result<PathBuf, LLMError> {
    if let Ok(path) = std::env::var("QMT_PROVIDER_CACHE_DIR")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = std::env::var("QMT_CONFIG_DIR")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = std::env::var("QMT_HOME")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    dirs::home_dir()
        .map(|home| home.join(".qmt"))
        .ok_or_else(|| {
            LLMError::GenericError("Could not determine QueryMT provider cache directory".into())
        })
}

fn provider_cache_path() -> Result<PathBuf, LLMError> {
    Ok(provider_cache_dir()?.join(CACHE_FILE))
}

fn is_cache_fresh(file_path: &Path) -> bool {
    if let Ok(metadata) = fs::metadata(file_path)
        && let Ok(modified) = metadata.modified()
        && let Ok(modified_time) = modified.duration_since(UNIX_EPOCH)
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        return now.as_secs() - modified_time.as_secs() < CACHE_DURATION;
    }
    false
}

async fn download_and_cache_providers(file_path: &Path) -> Result<ProvidersRegistry, LLMError> {
    let client = Client::new();
    let response = client.get(API_URL).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await?.to_vec();
        return Err(crate::error::classify_http_status(
            status.as_u16(),
            &headers,
            &body,
        ));
    }

    // API returns a top-level map of providers, convert into ProvidersRegistry
    let map = response
        .json::<std::collections::HashMap<String, super::types::ProviderInfo>>()
        .await?;

    let registry: ProvidersRegistry = map.into();

    let json = serde_json::to_string(&registry)?;
    fs::create_dir_all(file_path.parent().unwrap())?;
    let mut file = File::create(file_path)?;
    file.write_all(json.as_bytes())?;

    Ok(registry)
}

pub fn read_providers_from_cache() -> Result<ProvidersRegistry, LLMError> {
    let file_path = provider_cache_path()?;

    let mut file = File::open(file_path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let registry: ProvidersRegistry = serde_json::from_str(&contents)?;
    Ok(registry)
}

pub async fn update_providers_if_stale() -> Result<bool, LLMError> {
    let file_path = provider_cache_path()?;

    if is_cache_fresh(&file_path) {
        return Ok(false);
    }

    download_and_cache_providers(&file_path).await?;
    Ok(true)
}
