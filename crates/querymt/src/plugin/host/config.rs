use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[cfg(feature = "extism_host")]
use super::oci::OciDownloaderConfig;

#[derive(Debug, Deserialize)]
pub struct PluginConfig {
    pub providers: Vec<ProviderConfig>,
    #[cfg(feature = "extism_host")]
    pub oci: Option<OciDownloaderConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub path: String,
    pub config: Option<HashMap<String, toml::Value>>,
}

impl PluginConfig {
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p: &Path = path.as_ref();
        if !p.exists() {
            return Err(anyhow::anyhow!(
                "Config file not found at: {}. Please create a config file first.",
                p.display()
            ));
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");

        let content = fs::read_to_string(p)
            .with_context(|| format!("Failed to read config file at {}", p.display()))?;

        let config = match ext {
            "json" => serde_json::from_str(&content)?,
            "yaml" | "yml" => serde_yaml::from_str(&content)?,
            "toml" => toml::from_str(&content)?,
            _ => return Err(anyhow::anyhow!("Unsupported config format: {}", ext)),
        };

        Ok(config)
    }

    pub fn default_path() -> Result<PathBuf> {
        if let Ok(path) = std::env::var("QMT_PROVIDER_CONFIG")
            && !path.trim().is_empty()
        {
            return Ok(PathBuf::from(path));
        }

        let mut roots = Vec::new();
        if let Ok(path) = std::env::var("QMT_CONFIG_DIR")
            && !path.trim().is_empty()
        {
            roots.push(PathBuf::from(path));
        }
        if let Ok(path) = std::env::var("QMT_HOME")
            && !path.trim().is_empty()
        {
            roots.push(PathBuf::from(path));
        }
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".qmt"));
        }

        for root in roots {
            for candidate in [
                root.join("providers.toml"),
                root.join("providers.json"),
                root.join("providers.yaml"),
                root.join("providers.yml"),
            ] {
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }

        Err(anyhow::anyhow!(
            "No provider config found. Set QMT_PROVIDER_CONFIG or create ~/.qmt/providers.toml"
        ))
    }

    pub fn from_default_path() -> Result<Self> {
        let path = Self::default_path()?;
        Self::from_path(path)
    }
}
