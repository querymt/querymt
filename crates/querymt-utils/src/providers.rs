use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

const LATEST_PROVIDERS_URL: &str = "https://repo.query.mt/latest.json";
const STABLE_PROVIDERS_URL: &str = "https://repo.query.mt/stable.json";

fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

fn is_stable_release_version(version: &str) -> bool {
    let version = version.strip_prefix('v').unwrap_or(version);
    let mut parts = version.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(major), Some(minor), Some(patch), None)
            if is_ascii_digits(major) && is_ascii_digits(minor) && is_ascii_digits(patch)
    )
}

pub fn default_remote_url_for_version(version: &str) -> &'static str {
    if is_stable_release_version(version) {
        STABLE_PROVIDERS_URL
    } else {
        LATEST_PROVIDERS_URL
    }
}

pub fn default_remote_url() -> String {
    default_remote_url_for_version(crate::BUILD_VERSION).to_string()
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

    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory found and QMT_HOME is not set"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_release_versions_use_stable_url() {
        assert_eq!(
            default_remote_url_for_version("0.4.2"),
            STABLE_PROVIDERS_URL
        );
        assert_eq!(
            default_remote_url_for_version("v0.4.2"),
            STABLE_PROVIDERS_URL
        );
    }

    #[test]
    fn non_release_versions_use_latest_url() {
        assert_eq!(
            default_remote_url_for_version("0.4.2-3-gabcdef"),
            LATEST_PROVIDERS_URL
        );
        assert_eq!(
            default_remote_url_for_version("0.4.2-dirty"),
            LATEST_PROVIDERS_URL
        );
        assert_eq!(
            default_remote_url_for_version("0.5.0-beta.1"),
            LATEST_PROVIDERS_URL
        );
        assert_eq!(
            default_remote_url_for_version("abcdef"),
            LATEST_PROVIDERS_URL
        );
        assert_eq!(default_remote_url_for_version("0.4"), LATEST_PROVIDERS_URL);
        assert_eq!(default_remote_url_for_version(""), LATEST_PROVIDERS_URL);
    }
}
