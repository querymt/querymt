use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

const DEFAULT_PROVIDERS_URL: &str = "https://repo.query.mt/nightly.json";

pub fn default_remote_url() -> String {
    std::env::var("QMT_PROVIDERS_URL").unwrap_or_else(|_| DEFAULT_PROVIDERS_URL.to_string())
}

pub async fn fetch_providers_repository(url_override: Option<String>) -> Result<String> {
    let url = url_override.unwrap_or_else(default_remote_url);
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch providers JSON from {url}"))?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Failed to download providers JSON from {url}: HTTP {}",
            response.status()
        ));
    }

    response
        .text()
        .await
        .with_context(|| format!("Failed to read providers JSON body from {url}"))
}

pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("No home directory found"))?;
    Ok(home.join(".qmt"))
}

/// Find a config file in the user's home .qmt directory
pub fn find_config_in_home(filenames: &[&str]) -> Result<PathBuf> {
    let cfg_dir = config_dir()?;
    for filename in filenames {
        let candidate = cfg_dir.join(filename);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    let cfg_dir = config_dir().unwrap_or_else(|_| PathBuf::from("~/.qmt"));
    Err(anyhow!("No config file found in {:?}", cfg_dir))
}

pub async fn get_providers_config(provider_config: Option<String>) -> Result<PathBuf> {
    let cfg_path = provider_config.as_deref().map(PathBuf::from).or_else(|| {
        find_config_in_home(&["providers.toml", "providers.json", "providers.yaml"]).ok()
    });

    match cfg_path {
        Some(path) => Ok(path),
        None => {
            let cfg_dir = config_dir()?;
            std::fs::create_dir_all(&cfg_dir)
                .with_context(|| format!("Failed to create config dir at {:?}", cfg_dir))?;
            let target = cfg_dir.join("providers.json");
            let contents = fetch_providers_repository(None).await?;
            std::fs::write(&target, contents)
                .with_context(|| format!("Failed to write providers config to {:?}", target))?;
            Ok(target)
        }
    }
}
